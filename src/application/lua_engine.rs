use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mlua::{Error as LuaError, HookTriggers, Lua, Value, VmState};

use crate::application::cache_key::sha256_hex;
use crate::domain::errors::{log_domain_error, ProxyError};
use crate::domain::models::{ProxyContext, ScriptingConfig, WebhookRequest};
use crate::domain::services::{LuaExecutor, WebhookFetcher};

const REDOS_TIMEOUT_MS: u64 = 100;
/// Cada cuántas instrucciones de la VM se comprueba el deadline. Suficientemente pequeño para
/// que un bucle cerrado se detenga cerca de `LUA_TIMEOUT_MS` y suficientemente grande para no
/// leer el reloj en cada instrucción (`docs/RUST_STYLE_GUIDE.md` § Sandbox de Lua).
const LUA_HOOK_INSTRUCTIONS: u32 = 2_000;
/// Respaldo del `tokio::time::timeout` exterior sobre el presupuesto del script. El mecanismo
/// primario es el hook; esto solo cubre un script bloqueado dentro de una llamada nativa.
const LUA_DEADLINE_GRACE_MS: u64 = 100;
/// Margen del `recv_timeout` del puente sobre el timeout del webhook: manda el deadline del
/// fetch, el margen solo cubre la entrega del resultado por el canal.
const WEBHOOK_BRIDGE_MARGIN_MS: u64 = 250;

/// Estado de una sola ejecución del sandbox.
struct RunState {
    deadline: Instant,
    timeout_ms: u64,
    /// Error de seguridad que el script no puede negociar. `proxy.http_request` corre en el hilo
    /// de la VM y no tiene forma de devolver un `ProxyError` a través de mlua, así que lo deja
    /// aquí y `execute_sync` lo propaga al terminar el `call`. Es *sticky*: un `pcall` no puede
    /// lavar un `ssrf_blocked` (`docs/BDD.md`, Feature 6).
    blocked: Mutex<Option<ProxyError>>,
    deadline_hit: AtomicBool,
}

impl RunState {
    fn new(timeout_ms: u64) -> Self {
        Self {
            deadline: Instant::now() + Duration::from_millis(timeout_ms),
            timeout_ms,
            blocked: Mutex::new(None),
            deadline_hit: AtomicBool::new(false),
        }
    }

    fn take_blocked(&self) -> Option<ProxyError> {
        self.blocked
            .lock()
            .map(|mut slot| slot.take())
            .unwrap_or_else(|poisoned| poisoned.into_inner().take())
    }
}

#[derive(Clone)]
pub struct SandboxedLuaEngine {
    timeout_ms: u64,
    memory_limit_mb: u32,
    webhook_timeout_ms: u64,
    webhook_fetcher: Arc<dyn WebhookFetcher>,
}

impl SandboxedLuaEngine {
    pub fn new(
        timeout_ms: u64,
        memory_limit_mb: u32,
        webhook_timeout_ms: u64,
        webhook_fetcher: Arc<dyn WebhookFetcher>,
    ) -> Self {
        Self {
            timeout_ms,
            memory_limit_mb,
            webhook_timeout_ms,
            webhook_fetcher,
        }
    }

    fn create_sandbox() -> Result<Lua, ProxyError> {
        let lua = Lua::new();

        lua.load(
            r#"
            os = nil
            io = nil
            package = nil
            debug = nil
            dofile = nil
            loadfile = nil
            load = nil
        "#,
        )
        .exec()
        .map_err(|e| ProxyError::LuaSandboxViolation {
            attempted_function: format!("sandbox_init: {}", e),
        })?;

        Ok(lua)
    }

    fn inject_proxy_table(
        lua: &Lua,
        context: &ProxyContext,
        webhook_timeout_ms: u64,
        webhook_fetcher: &Arc<dyn WebhookFetcher>,
        state: &Arc<RunState>,
    ) -> Result<(), ProxyError> {
        let proxy_table = lua.create_table().map_err(|e| ProxyError::Internal {
            reason: format!("Failed to create proxy table: {}", e),
        })?;

        let crypt_id = context.crypt_id.clone();
        let internal_id = context.internal_id.clone();
        let log_fn = lua
            .create_function(move |_, (level, message): (String, String)| {
                match level.as_str() {
                    "error" => {
                        tracing::error!(crypt_id = %crypt_id, internal_id = %internal_id, "{}", message);
                    }
                    "warn" => {
                        tracing::warn!(crypt_id = %crypt_id, internal_id = %internal_id, "{}", message);
                    }
                    "info" => {
                        tracing::info!(crypt_id = %crypt_id, internal_id = %internal_id, "{}", message);
                    }
                    _ => {
                        tracing::debug!(crypt_id = %crypt_id, internal_id = %internal_id, "{}", message);
                    }
                }
                Ok(())
            })
            .map_err(|e| ProxyError::Internal {
                reason: format!("Failed to create log function: {}", e),
            })?;

        proxy_table
            .set("log", log_fn)
            .map_err(|e| ProxyError::Internal {
                reason: format!("Failed to set log: {}", e),
            })?;

        let regex_fn = lua
            .create_function(
                |_, (input, pattern, replacement): (String, String, String)| {
                    let start = Instant::now();

                    let re = regex::Regex::new(&pattern)
                        .map_err(|e| LuaError::external(format!("Invalid regex pattern: {}", e)))?;

                    if start.elapsed() > Duration::from_millis(REDOS_TIMEOUT_MS) {
                        return Err(LuaError::external(
                            "ReDoS blocked: regex compilation exceeded timeout",
                        ));
                    }

                    let result = re.replace_all(&input, replacement.as_str()).to_string();

                    if start.elapsed() > Duration::from_millis(REDOS_TIMEOUT_MS * 5) {
                        return Err(LuaError::external(
                            "ReDoS blocked: regex replacement exceeded timeout",
                        ));
                    }

                    Ok(result)
                },
            )
            .map_err(|e| ProxyError::Internal {
                reason: format!("Failed to create regex_replace function: {}", e),
            })?;

        proxy_table
            .set("regex_replace", regex_fn)
            .map_err(|e| ProxyError::Internal {
                reason: format!("Failed to set regex_replace: {}", e),
            })?;

        // `proxy.http_request` es **síncrona** a propósito. Una `create_async_function` de mlua
        // solo puede reanudar desde una coroutine, y el cuerpo del script no es una coroutine:
        // llamarla devolvía siempre `attempt to yield from outside a coroutine`, así que el
        // webhook nunca llegó a ejecutarse. El puente lanza el fetch validado al runtime y espera
        // el resultado por canal, lo que además deja la VM sincronizada y, por tanto,
        // interrumpible por el hook de deadline.
        let fetcher = Arc::clone(webhook_fetcher);
        let whitelist = context.config.whitelist.clone();
        let bridge_state = Arc::clone(state);
        let http_fn = lua
            .create_function(
                move |lua,
                      (url, method, body, custom_timeout): (
                    String,
                    Option<String>,
                    Option<String>,
                    Option<u64>,
                )| {
                    let timeout_ms = custom_timeout
                        .unwrap_or(webhook_timeout_ms)
                        .min(webhook_timeout_ms);
                    let request = WebhookRequest {
                        url: url.clone(),
                        method: method
                            .unwrap_or_else(|| "GET".to_string())
                            .to_ascii_uppercase(),
                        body: body.map(String::into_bytes),
                        timeout_ms,
                        whitelist: whitelist.clone(),
                    };

                    let handle = match tokio::runtime::Handle::try_current() {
                        Ok(handle) => handle,
                        Err(_) => {
                            return Err(store_webhook_error(
                                &bridge_state,
                                ProxyError::WebhookFailed {
                                    url,
                                    reason: "no tokio runtime on this thread".to_string(),
                                },
                            ))
                        }
                    };

                    let (sender, receiver) = std::sync::mpsc::channel();
                    let fetcher = Arc::clone(&fetcher);
                    handle.spawn(async move {
                        let outcome = fetcher.fetch(&request).await;
                        let _ = sender.send(outcome);
                    });

                    let wait = Duration::from_millis(timeout_ms + WEBHOOK_BRIDGE_MARGIN_MS);
                    match receiver.recv_timeout(wait) {
                        Ok(Ok(response)) => {
                            let result_table = lua
                                .create_table()
                                .map_err(|e| LuaError::external(e.to_string()))?;
                            result_table
                                .set("status", response.status)
                                .map_err(|e| LuaError::external(e.to_string()))?;
                            // El cuerpo viaja como texto: el contrato del sandbox es destringible
                            // y los webhooks reales devuelven JSON o texto plano.
                            result_table
                                .set("body", String::from_utf8_lossy(&response.body).to_string())
                                .map_err(|e| LuaError::external(e.to_string()))?;
                            Ok(result_table)
                        }
                        Ok(Err(error)) => Err(store_webhook_error(&bridge_state, error)),
                        Err(_) => Err(store_webhook_error(
                            &bridge_state,
                            ProxyError::WebhookTimeout {
                                webhook_url: url,
                                timeout_ms,
                            },
                        )),
                    }
                },
            )
            .map_err(|e| ProxyError::Internal {
                reason: format!("Failed to create http_request function: {}", e),
            })?;

        proxy_table
            .set("http_request", http_fn)
            .map_err(|e| ProxyError::Internal {
                reason: format!("Failed to set http_request: {}", e),
            })?;

        lua.globals()
            .set("proxy", proxy_table)
            .map_err(|e| ProxyError::Internal {
                reason: format!("Failed to set global proxy: {}", e),
            })?;

        Ok(())
    }

    fn execute_sync(
        &self,
        script: &str,
        body: &[u8],
        context: &ProxyContext,
    ) -> Result<Vec<u8>, ProxyError> {
        let lua = Self::create_sandbox()?;
        let state = Arc::new(RunState::new(self.timeout_ms));
        Self::inject_proxy_table(
            &lua,
            context,
            self.webhook_timeout_ms,
            &self.webhook_fetcher,
            &state,
        )?;

        lua.set_memory_limit(self.memory_limit_mb as usize * 1024 * 1024)
            .map_err(|e| ProxyError::Internal {
                reason: format!("Failed to set memory limit: {}", e),
            })?;

        // Deadline real: el hook se evalúa cada `LUA_HOOK_INSTRUCTIONS` instrucciones y aborta la
        // VM cuando se superó `LUA_TIMEOUT_MS`. Antes se medía `elapsed` **después** del `call`,
        // así que un `while true do end` nunca devolvía el control y el thread `spawn_blocking`
        // quedaba quemado hasta que mataban el proceso.
        let hook_state = Arc::clone(&state);
        lua.set_global_hook(
            HookTriggers::new().every_nth_instruction(LUA_HOOK_INSTRUCTIONS),
            move |_lua, _debug| {
                if Instant::now() >= hook_state.deadline {
                    hook_state.deadline_hit.store(true, Ordering::Relaxed);
                    return Err(LuaError::RuntimeError(format!(
                        "lua deadline exceeded: {}ms",
                        hook_state.timeout_ms
                    )));
                }
                Ok(VmState::Continue)
            },
        )
        .map_err(|e| ProxyError::Internal {
            reason: format!("Failed to install Lua deadline hook: {e}"),
        })?;

        let body_str = String::from_utf8_lossy(body).to_string();

        let chunk = if script.trim_start().starts_with("return ")
            || script.trim_start().starts_with("return\t")
        {
            script.to_string()
        } else {
            format!("return {}", script)
        };

        let user_func = match lua.load(&chunk).eval::<mlua::Function>() {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("Lua script did not compile: {}, returning original body", e);
                return Ok(body.to_vec());
            }
        };

        let start = Instant::now();
        let result = user_func.call::<Value>(body_str);

        // El bloqueo de seguridad gana aunque el script capturara el error con `pcall`.
        if let Some(blocked) = state.take_blocked() {
            return Err(blocked);
        }

        if state.deadline_hit.load(Ordering::Relaxed) {
            return Err(ProxyError::ScriptTimeout {
                timeout_ms: self.timeout_ms,
                elapsed_ms: start.elapsed().as_millis() as u64,
            });
        }

        match result {
            Ok(Value::String(s)) => Ok(s.as_bytes().to_vec()),
            Ok(_) => Ok(body.to_vec()),
            Err(e) => {
                let err_msg = e.to_string();
                if err_msg.contains("attempt to call a nil value")
                    || err_msg.contains("attempt to index a nil value")
                {
                    tracing::warn!(
                        event = "lua_sandbox_violation",
                        error = %err_msg,
                        "Lua sandbox violation"
                    );
                    return Err(ProxyError::LuaSandboxViolation {
                        attempted_function: err_msg,
                    });
                }

                if err_msg.contains("memory") {
                    return Err(ProxyError::ScriptMemoryLimit {
                        memory_limit_mb: self.memory_limit_mb,
                        used_mb: self.memory_limit_mb,
                    });
                }

                tracing::warn!(error = %err_msg, "Lua script error, returning original body");
                Ok(body.to_vec())
            }
        }
    }
}

/// Registra el fallo del webhook por el diccionario y devuelve el error que ve el script. Los
/// dos códigos de seguridad además se guardan en `blocked`: abortan la petición ocurra lo que
/// ocurra dentro del script. El mensaje expone solo el `error_code`, nunca el motivo interno.
fn store_webhook_error(state: &Arc<RunState>, error: ProxyError) -> LuaError {
    let code = error.to_error_code();
    log_domain_error(&error);

    if matches!(
        error,
        ProxyError::SsrfBlocked { .. } | ProxyError::DomainNotWhitelisted { .. }
    ) {
        if let Ok(mut slot) = state.blocked.lock() {
            if slot.is_none() {
                *slot = Some(error);
            }
        }
        return LuaError::external(format!("webhook rejected: {code}"));
    }

    LuaError::external(format!("webhook failed: {code}"))
}

/// `docs/spec.md` Fase 4: el código solo se ejecuta si su digest coincide con el `code_hash`
/// declarado. Sin `code_hash` no hay nada con qué comparar (configs anteriores al contrato), así
/// que se ejecuta y la verificación queda documentada como deuda en `docs/DIAGRAMS.md`.
fn verify_script_integrity(scripting: &ScriptingConfig) -> Result<(), ProxyError> {
    let expected = match scripting.code_hash.strip_prefix("sha256:") {
        Some(digest) => digest.to_ascii_lowercase(),
        None => {
            if scripting.code_hash.is_empty() {
                return Ok(());
            }
            scripting.code_hash.to_ascii_lowercase()
        }
    };

    let actual = sha256_hex(&scripting.code);
    if expected != actual {
        return Err(ProxyError::IntegrityCheckFailed {
            resource_type: "scripting.code".to_string(),
            expected_hash: scripting.code_hash.clone(),
            actual_hash: format!("sha256:{actual}"),
        });
    }

    Ok(())
}

#[async_trait]
impl LuaExecutor for SandboxedLuaEngine {
    async fn execute(
        &self,
        _script: &str,
        body: &[u8],
        context: &ProxyContext,
    ) -> Result<Vec<u8>, ProxyError> {
        if body.len() as u64 > context.config.max_scripting_body_bytes {
            tracing::debug!(
                body_size = body.len(),
                max_bytes = context.config.max_scripting_body_bytes,
                "Body exceeds max_scripting_body_bytes, bypassing Lua"
            );
            return Ok(body.to_vec());
        }

        if !context.config.scripting.enabled || context.config.scripting.code.is_empty() {
            return Ok(body.to_vec());
        }

        verify_script_integrity(&context.config.scripting)?;

        let engine = self.clone();
        let script = context.config.scripting.code.clone();
        let body_vec = body.to_vec();
        let context_clone = context.clone();

        let start = Instant::now();
        let task = tokio::task::spawn_blocking(move || {
            engine.execute_sync(&script, &body_vec, &context_clone)
        });
        let budget = Duration::from_millis(self.timeout_ms + LUA_DEADLINE_GRACE_MS);

        match tokio::time::timeout(budget, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(join_error)) => Err(ProxyError::Internal {
                reason: format!("spawn_blocking join: {join_error}"),
            }),
            Err(_) => {
                tracing::warn!(
                    "Lua execution outlived its deadline: the VM hook did not stop it (blocked inside a native call)"
                );
                Err(ProxyError::ScriptTimeout {
                    timeout_ms: self.timeout_ms,
                    elapsed_ms: start.elapsed().as_millis() as u64,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{
        ClientConfig, ErrorHandlingConfig, ErrorMode, RateLimitConfig, ScriptingUpdate,
        WebhookResponse,
    };
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Devuelve lo que se le ponga por `respuesta` y recuerda las órdenes recibidas, para poder
    /// probar el puente del sandbox sin tocar la red.
    struct StubFetcher {
        respuesta: Result<(), &'static str>,
        /// Retardo de `fetch`, para ejercitar el `recv_timeout` del puente sin tocar la red.
        retraso: Duration,
        recibidas: Mutex<Vec<WebhookRequest>>,
    }

    impl StubFetcher {
        fn ok() -> Self {
            Self {
                respuesta: Ok(()),
                retraso: Duration::ZERO,
                recibidas: Mutex::new(Vec::new()),
            }
        }

        fn lento(retraso: Duration) -> Self {
            Self {
                respuesta: Ok(()),
                retraso,
                recibidas: Mutex::new(Vec::new()),
            }
        }

        fn falla_como(error: ProxyError) -> Self {
            Self {
                respuesta: Err(match error {
                    ProxyError::SsrfBlocked { .. } => "ssrf",
                    _ => "otro",
                }),
                retraso: Duration::ZERO,
                recibidas: Mutex::new(Vec::new()),
            }
        }

        fn last(&self) -> Option<WebhookRequest> {
            self.recibidas.lock().unwrap().last().cloned()
        }
    }

    #[async_trait]
    impl WebhookFetcher for StubFetcher {
        async fn fetch(&self, request: &WebhookRequest) -> Result<WebhookResponse, ProxyError> {
            self.recibidas.lock().unwrap().push(request.clone());
            if !self.retraso.is_zero() {
                tokio::time::sleep(self.retraso).await;
            }
            match self.respuesta {
                Ok(()) => Ok(WebhookResponse {
                    status: 200,
                    body: b"{\"ok\":true}".to_vec(),
                }),
                Err("ssrf") => Err(ProxyError::SsrfBlocked {
                    url: request.url.clone(),
                    resolved_ip: "169.254.169.254".to_string(),
                    reason: "DNS resolved to private IP".to_string(),
                }),
                Err(_) => Err(ProxyError::WebhookFailed {
                    url: request.url.clone(),
                    reason: "stub".to_string(),
                }),
            }
        }
    }

    fn test_context() -> ProxyContext {
        ProxyContext {
            crypt_id: "test12345678".to_string(),
            internal_id: "client_int_test".to_string(),
            config_version: 1,
            target_url: url::Url::parse("https://example.com/test").unwrap(),
            mime_hint: None,
            config: ClientConfig {
                id: "client_int_test".to_string(),
                rev: None,
                r#type: "client_config".to_string(),
                internal_id: "client_int_test".to_string(),
                crypt_id: "test12345678".to_string(),
                bearer_token_hash: "sha256:test".to_string(),
                config_version: 1,
                whitelist: vec!["example.com".to_string()],
                rate_limit: RateLimitConfig {
                    max_requests: 50,
                    window_seconds: 60,
                },
                max_scripting_body_bytes: 5 * 1024 * 1024,
                scripting: ScriptingConfig {
                    enabled: true,
                    code: String::new(),
                    code_hash: String::new(),
                },
                error_handling: ErrorHandlingConfig {
                    mode: ErrorMode::Wrapped,
                    fallback_urls: HashMap::new(),
                },
            },
        }
    }

    fn engine() -> SandboxedLuaEngine {
        SandboxedLuaEngine::new(200, 50, 5_000, Arc::new(StubFetcher::ok()))
    }

    #[test]
    fn test_sandbox_blocks_os() {
        let ctx = test_context();
        let script = r#"function(body) return os.execute("echo hacked") end"#;
        let result = engine().execute_sync(script, b"hello", &ctx);
        assert!(matches!(
            result,
            Err(ProxyError::LuaSandboxViolation { .. })
        ));
    }

    #[test]
    fn test_sandbox_blocks_io() {
        let ctx = test_context();
        let script = r#"function(body) local f = io.open("/etc/passwd") return "hacked" end"#;
        let result = engine().execute_sync(script, b"hello", &ctx);
        assert!(matches!(
            result,
            Err(ProxyError::LuaSandboxViolation { .. })
        ));
    }

    #[test]
    fn test_simple_transform() {
        let ctx = test_context();
        let script = r#"function(body) return string.upper(body) end"#;
        let result = engine().execute_sync(script, b"hello world", &ctx).unwrap();
        assert_eq!(String::from_utf8_lossy(&result), "HELLO WORLD");
    }

    #[test]
    fn test_regex_replace() {
        let ctx = test_context();
        let script = r#"
            function(body)
                return proxy.regex_replace(body, "\\d", "X")
            end
        "#;
        let result = engine()
            .execute_sync(script, b"order 12345 confirmed", &ctx)
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&result), "order XXXXX confirmed");
    }

    #[test]
    fn test_body_bypass_in_execute() {
        let mut ctx = test_context();
        ctx.config.max_scripting_body_bytes = 5;
        ctx.config.scripting.enabled = true;
        ctx.config.scripting.code = "function(body) return 'replaced' end".to_string();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let body = b"this body is larger than 5 bytes";
        let result = rt
            .block_on(engine().execute("whatever", body, &ctx))
            .unwrap();
        assert_eq!(result, body.to_vec());
    }

    #[test]
    fn test_disabled_scripting_returns_original() {
        let mut ctx = test_context();
        ctx.config.scripting.enabled = false;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let body = b"original content";
        let result = rt
            .block_on(engine().execute("function(b) return 'x' end", body, &ctx))
            .unwrap();
        assert_eq!(result, body.to_vec());
    }

    #[test]
    fn test_error_returns_original_body() {
        let ctx = test_context();
        let script = r#"function(body) error("intentional error") end"#;
        let result = engine().execute_sync(script, b"original", &ctx).unwrap();
        assert_eq!(String::from_utf8_lossy(&result), "original");
    }

    #[test]
    fn test_proxy_log_does_not_crash() {
        let ctx = test_context();
        let script = r#"
            function(body)
                proxy.log("info", "processing request")
                proxy.log("warn", "something happened")
                proxy.log("error", "an error occurred")
                return body
            end
        "#;
        let result = engine().execute_sync(script, b"test", &ctx).unwrap();
        assert_eq!(String::from_utf8_lossy(&result), "test");
    }

    #[test]
    fn test_identity_passthrough() {
        let ctx = test_context();
        let script = r#"function(body) return body end"#;
        let result = engine().execute_sync(script, b"unchanged", &ctx).unwrap();
        assert_eq!(String::from_utf8_lossy(&result), "unchanged");
    }

    /// Regresión del P1: `proxy.http_request` era una `create_async_function` y mlua la rechazaba
    /// con `attempt to yield from outside a coroutine`, así que ningún script pudo llamarla nunca.
    #[tokio::test]
    async fn test_http_request_bridge_llama_al_fetcher() {
        let fetcher = Arc::new(StubFetcher::ok());
        let engine = SandboxedLuaEngine::new(2_000, 50, 5_000, Arc::clone(&fetcher) as Arc<_>);
        let mut ctx = test_context();
        ctx.config.scripting.code = r#"
            function(body)
                local res = proxy.http_request("https://example.com/hook", "POST", "hola")
                return body .. "|" .. res.status .. "|" .. res.body
            end
        "#
        .to_string();

        let result = engine
            .execute("", b"payload", &ctx)
            .await
            .expect("the sync bridge must reach the fetcher");

        assert_eq!(
            String::from_utf8_lossy(&result),
            r#"payload|200|{"ok":true}"#
        );
        let orden = fetcher.last().expect("the fetch must be dispatched");
        assert_eq!(orden.method, "POST");
        assert_eq!(orden.body, Some(b"hola".to_vec()));
        assert_eq!(orden.whitelist, ctx.config.whitelist);
    }

    /// Un `ssrf_blocked` del webhook aborta la petición aunque el script lo capture con `pcall`:
    /// el intento de evadir la whitelist no se puede lavar desde dentro del sandbox.
    #[tokio::test]
    async fn test_http_request_ssrf_propasa_aunque_el_script_lo_capture() {
        let engine = SandboxedLuaEngine::new(
            2_000,
            50,
            5_000,
            Arc::new(StubFetcher::falla_como(ProxyError::SsrfBlocked {
                url: "http://169.254.169.254/".to_string(),
                resolved_ip: "169.254.169.254".to_string(),
                reason: "private".to_string(),
            })) as Arc<_>,
        );
        let mut ctx = test_context();
        ctx.config.scripting.code = r#"
            function(body)
                local ok, err = pcall(proxy.http_request, "http://169.254.169.254/latest", "GET")
                return "capturado=" .. tostring(ok)
            end
        "#
        .to_string();

        let result = engine.execute("", b"original", &ctx).await;

        assert!(matches!(result, Err(ProxyError::SsrfBlocked { .. })));
    }

    /// Regresión del segundo P1: con `LUA_TIMEOUT_MS=200` un bucle cerrado tiene que devolverse
    /// el cuerpo original en un tiempo cercano al presupuesto, no quemar el thread hasta SIGKILL.
    #[tokio::test]
    async fn test_bucle_infinito_se_interrumpe_por_deadline() {
        let engine = engine();
        let mut ctx = test_context();
        ctx.config.scripting.code = "function(body) while true do end return body end".to_string();

        let start = Instant::now();
        let result = engine.execute("", b"original", &ctx).await;
        let elapsed = start.elapsed();

        assert!(
            matches!(
                result,
                Err(ProxyError::ScriptTimeout {
                    timeout_ms: 200,
                    ..
                })
            ),
            "expected ScriptTimeout, got {result:?}"
        );
        assert!(
            elapsed < Duration::from_millis(1_000),
            "deadline not enforced, waited {elapsed:?}"
        );
    }

    /// Escenario "Timeout en webhook desde Lua": un fetch que no cabe en `WEBHOOK_TIMEOUT_MS` tiene
    /// que cortarlo por el `recv_timeout` del puente, no esperar a que el origen responda. El script
    /// recibe el `error_code` y la petición sigue su curso.
    #[tokio::test]
    async fn test_http_request_lento_se_corta_por_webhook_timeout() {
        let fetcher = Arc::new(StubFetcher::lento(Duration::from_millis(800)));
        let engine = SandboxedLuaEngine::new(2_000, 50, 50, Arc::clone(&fetcher) as Arc<_>);
        let mut ctx = test_context();
        ctx.config.scripting.code = r#"
            function(body)
                local ok, err = pcall(proxy.http_request, "https://example.com/hook", "GET")
                return tostring(ok) .. "|" .. tostring(err)
            end
        "#
        .to_string();

        let start = Instant::now();
        let result = engine.execute("", b"original", &ctx).await.unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(700),
            "el puente esperó al origen en lugar de cortar: {elapsed:?}"
        );
        assert!(
            String::from_utf8_lossy(&result).contains("webhook failed: webhook_timeout"),
            "expected webhook_timeout, got {}",
            String::from_utf8_lossy(&result)
        );
        assert_eq!(
            fetcher
                .last()
                .expect("the fetch must be dispatched")
                .timeout_ms,
            50
        );
    }

    /// Un webhook lento o caído degrada, no aborta: el cliente sigue recibiendo contenido. Lo que
    /// sí aborta es un `ssrf_blocked` surgido del mismo script (ver el test anterior).
    #[tokio::test]
    async fn test_webhook_timeout_devuelve_el_cuerpo_original() {
        let fetcher = Arc::new(StubFetcher::lento(Duration::from_millis(800)));
        let engine = SandboxedLuaEngine::new(2_000, 50, 50, Arc::clone(&fetcher) as Arc<_>);
        let mut ctx = test_context();
        ctx.config.scripting.code = r#"
            function(body)
                local res = proxy.http_request("https://example.com/hook", "GET")
                return body .. "|" .. res.body
            end
        "#
        .to_string();

        let result = engine.execute("", b"original", &ctx).await.unwrap();

        assert_eq!(String::from_utf8_lossy(&result), "original");
    }

    #[test]
    fn verify_integrity_acepta_el_digest_del_codigo() {
        let code = "function(body) return body end".to_string();
        let scripting = ScriptingConfig {
            enabled: true,
            code_hash: format!("sha256:{}", sha256_hex(&code)),
            code,
        };
        assert!(verify_script_integrity(&scripting).is_ok());
    }

    #[test]
    fn verify_integrity_rechaza_un_digest_distinto() {
        let scripting = ScriptingConfig {
            enabled: true,
            code: "function(body) return body end".to_string(),
            code_hash: "sha256:deadbeef".to_string(),
        };
        let error = verify_script_integrity(&scripting)
            .expect_err("a mismatched digest must block execution");
        assert!(matches!(error, ProxyError::IntegrityCheckFailed { .. }));
    }

    #[test]
    fn verify_integrity_tolera_configs_sin_digest() {
        let scripting = ScriptingConfig {
            enabled: true,
            code: "function(body) return body end".to_string(),
            code_hash: String::new(),
        };
        assert!(verify_script_integrity(&scripting).is_ok());
    }

    #[test]
    fn un_digest_invalido_no_se_confunde_con_sin_digest() {
        let scripting = ScriptingConfig {
            enabled: true,
            code: "function(body) return body end".to_string(),
            code_hash: "no-es-un-hash".to_string(),
        };
        assert!(matches!(
            verify_script_integrity(&scripting),
            Err(ProxyError::IntegrityCheckFailed { .. })
        ));
    }

    #[test]
    fn el_script_debe_poder_actualizarse_con_code_hash_correcto() {
        // `ScriptingUpdate` es el payload del `PUT /config`: el hash que se guarda es el del
        // código que se va a ejecutar, así que la verificación del motor debe aceptarlo.
        let update = ScriptingUpdate {
            enabled: Some(true),
            code: Some("function(body) return string.upper(body) end".to_string()),
            code_hash: None,
        };
        let code = update.code.clone().unwrap();
        let scripting = ScriptingConfig {
            enabled: true,
            code_hash: format!("sha256:{}", sha256_hex(&code)),
            code,
        };
        assert!(verify_script_integrity(&scripting).is_ok());
    }
}
