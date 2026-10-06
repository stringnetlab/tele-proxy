//! Reglas de modificación de headers de respuesta, inspiradas en Cloudflare Ruleset Engine
//! (<https://developers.cloudflare.com/ruleset-engine/about/rules/>): cada regla tiene una
//! `expression` que se evalúa contra el contexto de la petición/respuesta y, si se cumple,
//! aplica operaciones `set`/`add`/`remove` sobre los headers de la respuesta.
//!
//! Vive en `domain/` porque ni el parser ni el evaluador dependen de Axum ni de Pingora: la
//! capa de interfaz traduce sus `HeaderMap` a `Vec<(String, String)>` y de vuelta.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Tipos de configuración
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderRule {
    /// Expresión estilo wirefilter (Cloudflare). Si evalúa a `true` se aplican las operaciones.
    pub expression: String,
    /// Cloudflare fija `set` como acción de las reglas de modificación de headers; el enum
    /// queda listo para futuras acciones (p. ej. `skip`). Otros valores se rechazan al
    /// deserializar el cuerpo del `PUT`.
    pub action: HeaderRuleAction,
    pub action_parameters: HeaderActionParameters,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeaderRuleAction {
    #[default]
    Set,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderActionParameters {
    pub headers: Vec<HeaderOperation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderOperation {
    /// Nombre del header. La validación lo normaliza a minúsculas y rechaza los headers que
    /// gestiona el proxy (framing, seguridad fija, `X-Cache`, ...).
    pub name: String,
    #[serde(default)]
    pub operation: HeaderOperationKind,
    /// Valor, con placeholders `${campo}` opcionales que se expanden contra el contexto.
    /// Requerido para `set`/`add`; ignorado en `remove`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeaderOperationKind {
    /// Reemplaza el valor (o crea el header).
    #[default]
    Set,
    /// Añade un valor; si el header ya existe, queda con varios (p. ej. varios `Set-Cookie`).
    Add,
    /// Elimina el header de la respuesta.
    Remove,
}

// ---------------------------------------------------------------------------
// Contexto de evaluación
// ---------------------------------------------------------------------------

/// Todo lo que una expresión puede observar. Los nombres de header van en minúsculas (formato
/// canónico de `HeaderMap`) y pueden repetirse para representar valores múltiples.
#[derive(Debug, Clone)]
pub struct RuleContext {
    pub method: String,
    pub url: url::Url,
    pub request_headers: Vec<(String, String)>,
    pub response_status: u16,
    pub response_headers: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// Valores y AST de expresiones
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    /// Un campo ausente (p. ej. un header que no vino). Ninguna comparación lo satisface.
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    Str,
    Int,
    Bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmpOp {
    Eq,
    Ne,
}

#[derive(Debug, Clone)]
enum Expr {
    Or(Vec<Expr>),
    And(Vec<Expr>),
    Not(Box<Expr>),
    Cmp(CmpOp, Term, Term),
    /// `<term> in { <valor> <valor> ... }` (membership, sintaxis de conjuntos de Cloudflare).
    In(Term, Vec<Value>),
    /// Término de tipo bool usado directo (p. ej. `starts_with(...)` sin comparación).
    Term(Term),
}

#[derive(Debug, Clone)]
enum Term {
    Field(FieldRef),
    Call {
        name: &'static str,
        ret: Ty,
        args: Vec<Term>,
    },
    Lit(Value),
}

impl Term {
    fn ty(&self) -> Ty {
        match self {
            Term::Field(field) => match field {
                FieldRef::HttpResponseStatus => Ty::Int,
                _ => Ty::Str,
            },
            Term::Call { ret, .. } => *ret,
            Term::Lit(value) => match value {
                Value::Str(_) => Ty::Str,
                Value::Int(_) => Ty::Int,
                Value::Bool(_) => Ty::Bool,
                Value::Null => Ty::Str,
            },
        }
    }
}

#[derive(Debug, Clone)]
enum FieldRef {
    HttpResponseStatus,
    HttpResponseContentType,
    HttpResponseHeader(String),
    HttpRequestMethod,
    HttpRequestHeader(String),
    UrlScheme,
    UrlHost,
    UrlPath,
    UrlQuery,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Int(i64),
    LParen,
    RParen,
    Comma,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Dot,
    Eq,
    Ne,
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

fn tokenize(src: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\n' | '\r' => i += 1,
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            '[' => {
                tokens.push(Token::LBracket);
                i += 1;
            }
            ']' => {
                tokens.push(Token::RBracket);
                i += 1;
            }
            '{' => {
                tokens.push(Token::LBrace);
                i += 1;
            }
            '}' => {
                tokens.push(Token::RBrace);
                i += 1;
            }
            '.' => {
                tokens.push(Token::Dot);
                i += 1;
            }
            '=' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Token::Eq);
                    i += 2;
                } else {
                    return Err(format!("expected '==' at position {i}"));
                }
            }
            '!' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Token::Ne);
                    i += 2;
                } else {
                    return Err(format!("unexpected '!' at position {i}"));
                }
            }
            '"' | '\'' => {
                let quote = c;
                let mut value = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        Some(&ch) if ch == quote => {
                            i += 1;
                            break;
                        }
                        Some('\\') => match chars.get(i + 1) {
                            Some(&'n') => {
                                value.push('\n');
                                i += 2;
                            }
                            Some(&'t') => {
                                value.push('\t');
                                i += 2;
                            }
                            Some(&q) if q == quote => {
                                value.push(q);
                                i += 2;
                            }
                            Some('\\') => {
                                value.push('\\');
                                i += 2;
                            }
                            _ => return Err("invalid escape sequence in string literal".into()),
                        },
                        Some(&ch) => {
                            value.push(ch);
                            i += 1;
                        }
                        None => return Err("unterminated string literal".into()),
                    }
                }
                tokens.push(Token::Str(value));
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                let n: i64 = text
                    .parse()
                    .map_err(|_| format!("invalid integer literal '{text}'"))?;
                tokens.push(Token::Int(n));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                tokens.push(Token::Ident(chars[start..i].iter().collect()));
            }
            _ => return Err(format!("unexpected character '{c}'")),
        }
    }

    Ok(tokens)
}

// ---------------------------------------------------------------------------
// Parser (descenso recursivo; precedencia: not > and > or)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgSpec {
    One,
    Two,
    Variadic,
}

/// Firma estática de cada función. Tipar en parseo, no en runtime: un `PUT` con
/// `len(200)` tiene que fallar en la validación, no en la primera petición.
const FUNCTIONS: &[(&str, ArgSpec, Ty)] = &[
    ("starts_with", ArgSpec::Two, Ty::Bool),
    ("ends_with", ArgSpec::Two, Ty::Bool),
    ("contains", ArgSpec::Two, Ty::Bool),
    ("matches", ArgSpec::Two, Ty::Bool),
    ("lower", ArgSpec::One, Ty::Str),
    ("upper", ArgSpec::One, Ty::Str),
    ("len", ArgSpec::One, Ty::Int),
    ("concat", ArgSpec::Variadic, Ty::Str),
];

fn function_signature(name: &str) -> Result<(&'static str, ArgSpec, Ty), String> {
    FUNCTIONS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(n, spec, ret)| (*n, *spec, *ret))
        .ok_or_else(|| format!("unknown function '{name}'"))
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<&Token> {
        let token = self.tokens.get(self.pos);
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn eat(&mut self, expected: &Token) -> bool {
        if self.peek() == Some(expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_ident(&mut self, keyword: &str) -> bool {
        match self.peek() {
            Some(Token::Ident(id)) if id == keyword => {
                self.pos += 1;
                true
            }
            _ => false,
        }
    }

    fn expect(&mut self, expected: &Token) -> Result<(), String> {
        if self.eat(expected) {
            Ok(())
        } else {
            Err(format!(
                "expected {:?} at token {}, found {:?}",
                expected,
                self.pos,
                self.peek()
            ))
        }
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut parts = vec![self.parse_and()?];
        while self.eat_ident("or") {
            parts.push(self.parse_and()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().expect("non-empty")
        } else {
            Expr::Or(parts)
        })
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut parts = vec![self.parse_unary()?];
        while self.eat_ident("and") {
            parts.push(self.parse_unary()?);
        }
        Ok(if parts.len() == 1 {
            parts.pop().expect("non-empty")
        } else {
            Expr::And(parts)
        })
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        if self.eat_ident("not") {
            return Ok(Expr::Not(Box::new(self.parse_unary()?)));
        }
        if self.eat(&Token::LParen) {
            let inner = self.parse_or()?;
            self.expect(&Token::RParen)?;
            return Ok(inner);
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Expr, String> {
        let left = self.parse_term()?;

        // Sintaxis de Cloudflare: `eq`/`ne`/`in`. `==`/`!=` se aceptan como alias cómodos, pero
        // la documentación y los ejemplos usan la forma canónica.
        let op = if self.eat_ident("eq") || self.eat(&Token::Eq) {
            Some(CmpOp::Eq)
        } else if self.eat_ident("ne") || self.eat(&Token::Ne) {
            Some(CmpOp::Ne)
        } else {
            None
        };

        match op {
            Some(op) => {
                let right = self.parse_term()?;
                if left.ty() != right.ty() {
                    return Err(format!(
                        "cannot compare {:?} with {:?}",
                        left.ty(),
                        right.ty()
                    ));
                }
                Ok(Expr::Cmp(op, left, right))
            }
            None if self.eat_ident("in") => {
                let values = self.parse_set(left.ty())?;
                Ok(Expr::In(left, values))
            }
            None => {
                if left.ty() != Ty::Bool {
                    return Err(format!(
                        "expression must evaluate to a boolean, found {:?}",
                        left.ty()
                    ));
                }
                Ok(Expr::Term(left))
            }
        }
    }

    /// Conjunto de Cloudflare: `{ "a" "b" 200 true }` (valores separados por espacios, sin
    /// comas). Todos los valores deben tener el tipo del término de la izquierda.
    fn parse_set(&mut self, expected: Ty) -> Result<Vec<Value>, String> {
        self.expect(&Token::LBrace)?;
        let mut values = Vec::new();
        loop {
            let value = match self.next() {
                Some(Token::Str(s)) => Value::Str(s.clone()),
                Some(Token::Int(n)) => Value::Int(*n),
                Some(Token::Ident(id)) if id == "true" => Value::Bool(true),
                Some(Token::Ident(id)) if id == "false" => Value::Bool(false),
                other => {
                    return Err(format!(
                        "expected a literal inside the set, found {other:?}"
                    ))
                }
            };
            let matches_type = matches!(
                (&value, expected),
                (Value::Str(_), Ty::Str) | (Value::Int(_), Ty::Int) | (Value::Bool(_), Ty::Bool)
            );
            if !matches_type {
                return Err(format!(
                    "set literal {:?} does not match the field type {:?}",
                    value, expected
                ));
            }
            values.push(value);
            if self.eat(&Token::RBrace) {
                break;
            }
        }
        if values.is_empty() {
            return Err("set cannot be empty".into());
        }
        Ok(values)
    }

    fn parse_term(&mut self) -> Result<Term, String> {
        let token = self
            .next()
            .ok_or_else(|| "unexpected end of expression".to_string())?
            .clone();

        match token {
            Token::Str(s) => Ok(Term::Lit(Value::Str(s))),
            Token::Int(n) => Ok(Term::Lit(Value::Int(n))),
            Token::Ident(id) => match id.as_str() {
                "true" => Ok(Term::Lit(Value::Bool(true))),
                "false" => Ok(Term::Lit(Value::Bool(false))),
                "and" | "or" | "not" => Err(format!("'{id}' cannot be used as a value")),
                _ => {
                    if self.eat(&Token::LParen) {
                        self.parse_call(id)
                    } else {
                        self.parse_field(id)
                    }
                }
            },
            other => Err(format!("expected a value, found {other:?}")),
        }
    }

    fn parse_call(&mut self, name: String) -> Result<Term, String> {
        let (name, spec, ret) = function_signature(&name)?;
        let mut args = Vec::new();
        if !self.eat(&Token::RParen) {
            loop {
                args.push(self.parse_term()?);
                if self.eat(&Token::Comma) {
                    continue;
                }
                self.expect(&Token::RParen)?;
                break;
            }
        }

        let arity_ok = match spec {
            ArgSpec::One => args.len() == 1,
            ArgSpec::Two => args.len() == 2,
            ArgSpec::Variadic => !args.is_empty(),
        };
        if !arity_ok {
            return Err(format!(
                "function '{name}' received an invalid number of arguments"
            ));
        }
        for arg in &args {
            if arg.ty() != Ty::Str {
                return Err(format!(
                    "function '{name}' expects string arguments, found {:?}",
                    arg.ty()
                ));
            }
        }

        Ok(Term::Call { name, ret, args })
    }

    fn parse_field(&mut self, first: String) -> Result<Term, String> {
        let mut path = vec![first];
        while self.eat(&Token::Dot) {
            match self.next() {
                Some(Token::Ident(part)) => path.push(part.clone()),
                other => return Err(format!("expected a field name after '.', found {other:?}")),
            }
        }

        let header_name = if self.eat(&Token::LBracket) {
            match self.next().cloned() {
                Some(Token::Str(name)) => {
                    self.expect(&Token::RBracket)?;
                    Some(name.to_ascii_lowercase())
                }
                other => {
                    return Err(format!(
                        "expected a string with the header name inside [], found {other:?}"
                    ))
                }
            }
        } else {
            None
        };

        Ok(Term::Field(resolve_field(&path, header_name)?))
    }
}

fn resolve_field(path: &[String], header_name: Option<String>) -> Result<FieldRef, String> {
    let part = |i: usize| path.get(i).map(String::as_str);
    match (part(0), part(1), part(2), header_name) {
        (Some("http"), Some("response"), Some("status"), None) if path.len() == 3 => {
            Ok(FieldRef::HttpResponseStatus)
        }
        (Some("http"), Some("response"), Some("content_type"), None) if path.len() == 3 => {
            Ok(FieldRef::HttpResponseContentType)
        }
        (Some("http"), Some("response"), Some("headers"), Some(name)) if path.len() == 3 => {
            Ok(FieldRef::HttpResponseHeader(name))
        }
        (Some("http"), Some("request"), Some("method"), None) if path.len() == 3 => {
            Ok(FieldRef::HttpRequestMethod)
        }
        (Some("http"), Some("request"), Some("headers"), Some(name)) if path.len() == 3 => {
            Ok(FieldRef::HttpRequestHeader(name))
        }
        (Some("url"), Some("scheme"), None, None) if path.len() == 2 => Ok(FieldRef::UrlScheme),
        (Some("url"), Some("host"), None, None) if path.len() == 2 => Ok(FieldRef::UrlHost),
        (Some("url"), Some("path"), None, None) if path.len() == 2 => Ok(FieldRef::UrlPath),
        (Some("url"), Some("query"), None, None) if path.len() == 2 => Ok(FieldRef::UrlQuery),
        _ => Err(format!("unknown field '{}'", path.join("."))),
    }
}

/// Parsea una expresión. Los errores son mensajes legibles para el `400 invalid_config`.
/// Es privada porque la API pública de validación es `validate_expression` y la de ejecución
/// `apply_header_rules`; el AST no se expone fuera del módulo.
fn parse_expression(src: &str) -> Result<Expr, String> {
    let tokens = tokenize(src)?;
    if tokens.is_empty() {
        return Err("expression is empty".into());
    }
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
    };
    let expr = parser.parse_or()?;
    if parser.pos != parser.tokens.len() {
        return Err(format!(
            "unexpected token {:?} after the end of the expression",
            parser.peek()
        ));
    }
    Ok(expr)
}

/// Validación en escritura (`PUT /config`): sintaxis, tipos estáticos y compilación de los
/// patrones regex literales de `matches()`. La evaluación en sí no se puede hacer aquí: un
/// header ausente del contexto de validación no convierte en inválida una expresión que en
/// runtime es correcta (semántica wirefilter: campo ausente → la comparación es falsa).
pub fn validate_expression(src: &str) -> Result<(), String> {
    let expr = parse_expression(src)?;
    check_regex_literals(&expr)
}

fn check_regex_literals(expr: &Expr) -> Result<(), String> {
    match expr {
        Expr::Or(parts) | Expr::And(parts) => {
            for part in parts {
                check_regex_literals(part)?;
            }
        }
        Expr::Not(inner) => check_regex_literals(inner)?,
        Expr::Cmp(_, left, right) => {
            check_matches_literal(left)?;
            check_matches_literal(right)?;
        }
        Expr::In(left, _) => check_matches_literal(left)?,
        Expr::Term(term) => check_matches_literal(term)?,
    }
    Ok(())
}

fn check_matches_literal(term: &Term) -> Result<(), String> {
    if let Term::Call { name, args, .. } = term {
        if *name == "matches" {
            if let Some(Term::Lit(Value::Str(pattern))) = args.get(1) {
                regex::Regex::new(pattern)
                    .map_err(|e| format!("invalid regex pattern '{pattern}': {e}"))?;
            }
        }
        for arg in args {
            check_matches_literal(arg)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Evaluación
// ---------------------------------------------------------------------------

impl Expr {
    /// Evalúa la expresión contra el contexto. Tras la validación en escritura no puede
    /// fallar: los únicos casos dinámicos (campo ausente) evalúan a `false`, como en wirefilter.
    pub fn eval(&self, ctx: &RuleContext) -> bool {
        match self {
            Expr::Or(parts) => parts.iter().any(|part| part.eval(ctx)),
            Expr::And(parts) => parts.iter().all(|part| part.eval(ctx)),
            Expr::Not(inner) => !inner.eval(ctx),
            Expr::Cmp(op, left, right) => {
                let (left, right) = (left.eval(ctx), right.eval(ctx));
                if left == Value::Null || right == Value::Null {
                    return false;
                }
                let equal = left == right;
                match op {
                    CmpOp::Eq => equal,
                    CmpOp::Ne => !equal,
                }
            }
            Expr::In(left, values) => {
                let left = left.eval(ctx);
                if left == Value::Null {
                    return false;
                }
                values.contains(&left)
            }
            Expr::Term(term) => matches!(term.eval(ctx), Value::Bool(true)),
        }
    }
}

impl Term {
    fn eval(&self, ctx: &RuleContext) -> Value {
        match self {
            Term::Lit(value) => value.clone(),
            Term::Field(field) => resolve_field_value(field, ctx),
            Term::Call { name, args, .. } => eval_call(name, args, ctx),
        }
    }
}

fn eval_call(name: &str, args: &[Term], ctx: &RuleContext) -> Value {
    let values: Vec<Value> = args.iter().map(|arg| arg.eval(ctx)).collect();

    // Semántica wirefilter: si un argumento es un campo ausente (Null), la llamada no matchea
    // en lugar de romper la regla entera.
    let two_strs = |values: &[Value]| -> Option<(String, String)> {
        match (&values[0], &values[1]) {
            (Value::Str(a), Value::Str(b)) => Some((a.clone(), b.clone())),
            _ => None,
        }
    };

    match name {
        "starts_with" => match two_strs(&values) {
            Some((s, prefix)) => Value::Bool(s.starts_with(&prefix)),
            None => Value::Bool(false),
        },
        "ends_with" => match two_strs(&values) {
            Some((s, suffix)) => Value::Bool(s.ends_with(&suffix)),
            None => Value::Bool(false),
        },
        "contains" => match two_strs(&values) {
            Some((s, needle)) => Value::Bool(s.contains(&needle)),
            None => Value::Bool(false),
        },
        // El crate `regex` no backtrackea: no hay riesgo ReDoS (ver `REDOS_TIMEOUT_MS`, que
        // protege el `proxy.regex_replace` de Lua, no hace falta aquí).
        "matches" => match two_strs(&values) {
            Some((s, pattern)) => Value::Bool(
                regex::Regex::new(&pattern)
                    .map(|re| re.is_match(&s))
                    .unwrap_or(false),
            ),
            None => Value::Bool(false),
        },
        "lower" => match &values[0] {
            Value::Str(s) => Value::Str(s.to_lowercase()),
            _ => Value::Null,
        },
        "upper" => match &values[0] {
            Value::Str(s) => Value::Str(s.to_uppercase()),
            _ => Value::Null,
        },
        "len" => match &values[0] {
            Value::Str(s) => Value::Int(s.chars().count() as i64),
            _ => Value::Null,
        },
        "concat" => Value::Str(
            values
                .iter()
                .map(|value| match value {
                    Value::Str(s) => s.clone(),
                    Value::Int(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    Value::Null => String::new(),
                })
                .collect(),
        ),
        _ => Value::Null,
    }
}

fn resolve_field_value(field: &FieldRef, ctx: &RuleContext) -> Value {
    match field {
        FieldRef::HttpResponseStatus => Value::Int(i64::from(ctx.response_status)),
        FieldRef::HttpResponseContentType => header_lookup(&ctx.response_headers, "content-type"),
        FieldRef::HttpResponseHeader(name) => header_lookup(&ctx.response_headers, name),
        FieldRef::HttpRequestMethod => Value::Str(ctx.method.clone()),
        FieldRef::HttpRequestHeader(name) => header_lookup(&ctx.request_headers, name),
        FieldRef::UrlScheme => Value::Str(ctx.url.scheme().to_string()),
        FieldRef::UrlHost => ctx
            .url
            .host_str()
            .map(|host| Value::Str(host.to_lowercase()))
            .unwrap_or(Value::Null),
        FieldRef::UrlPath => Value::Str(ctx.url.path().to_string()),
        FieldRef::UrlQuery => ctx
            .url
            .query()
            .map(|query| Value::Str(query.to_string()))
            .unwrap_or(Value::Null),
    }
}

fn header_lookup(headers: &[(String, String)], name: &str) -> Value {
    headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| Value::Str(value.clone()))
        .unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// Plantillas de valor `${campo}`
// ---------------------------------------------------------------------------

fn parse_field_placeholder(src: &str) -> Result<FieldRef, String> {
    let tokens = tokenize(src)?;
    let mut path: Vec<String> = Vec::new();
    let mut header_name = None;
    let mut i = 0;

    while i < tokens.len() {
        match &tokens[i] {
            Token::Ident(id) => {
                path.push(id.clone());
                i += 1;
            }
            Token::Dot => {
                i += 1;
            }
            Token::LBracket => match (tokens.get(i + 1), tokens.get(i + 2)) {
                (Some(Token::Str(name)), Some(Token::RBracket)) => {
                    header_name = Some(name.to_ascii_lowercase());
                    i += 3;
                }
                _ => return Err("invalid header access inside placeholder".into()),
            },
            other => return Err(format!("unexpected token {other:?} inside placeholder")),
        }
    }

    if path.is_empty() {
        return Err("empty field inside placeholder".into());
    }
    resolve_field(&path, header_name)
}

/// Expande los placeholders `${campo}` de un valor de header contra el contexto. Un campo
/// ausente (p. ej. un header que no vino en la request) expande a cadena vacía. Un placeholder
/// que no parsea se deja tal cual: la validación en `PUT` ya habrá rechazado esos valores.
pub fn expand_value_template(template: &str, ctx: &RuleContext) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let inner_start = start + 2;
        match rest[inner_start..].find('}') {
            Some(rel_end) => {
                let inner = &rest[inner_start..inner_start + rel_end];
                match parse_field_placeholder(inner) {
                    Ok(field) => match resolve_field_value(&field, ctx) {
                        Value::Str(s) => out.push_str(&s),
                        Value::Int(n) => out.push_str(&n.to_string()),
                        Value::Bool(b) => out.push_str(&b.to_string()),
                        Value::Null => {}
                    },
                    Err(_) => out.push_str(&rest[start..inner_start + rel_end + 1]),
                }
                rest = &rest[inner_start + rel_end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Validación en escritura de los placeholders de un valor: cada `${...}` tiene que ser un
/// campo conocido (mismos campos que admite la expresión).
pub fn validate_value_template(template: &str) -> Result<(), String> {
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        let inner_start = start + 2;
        match rest[inner_start..].find('}') {
            Some(rel_end) => {
                parse_field_placeholder(&rest[inner_start..inner_start + rel_end])?;
                rest = &rest[inner_start + rel_end + 1..];
            }
            None => return Err("unterminated '${' placeholder".into()),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Aplicación de reglas sobre la respuesta
// ---------------------------------------------------------------------------

/// Aplica las reglas del cliente sobre los pares `(header, valor)` de la respuesta, en orden.
/// Entre regla y regla se refresca la vista de los headers de respuesta del contexto, así cada
/// expresión ve las mutaciones que las reglas anteriores ya aplicaron (igual que las fases de
/// Cloudflare se ejecutan en orden); la primera regla siempre evalúa contra los headers crudos.
pub fn apply_header_rules(
    rules: &[HeaderRule],
    ctx: &RuleContext,
    headers: &mut Vec<(String, String)>,
) {
    let mut ctx = ctx.clone();

    for rule in rules {
        let expr = match parse_expression(&rule.expression) {
            Ok(expr) => expr,
            Err(reason) => {
                // No debería ocurrir: `PUT /config` valida cada expresión. Si llega (documento
                // escrito a mano en CouchDB), se salta la regla y queda en el log.
                tracing::warn!(
                    reason = %reason,
                    "header rule expression failed to parse at runtime; rule skipped"
                );
                continue;
            }
        };
        if !expr.eval(&ctx) {
            continue;
        }

        for op in &rule.action_parameters.headers {
            let name = op.name.to_ascii_lowercase();
            match op.operation {
                HeaderOperationKind::Set => {
                    let value = op
                        .value
                        .as_deref()
                        .map(|template| expand_value_template(template, &ctx))
                        .unwrap_or_default();
                    headers.retain(|(key, _)| key != &name);
                    headers.push((name, value));
                }
                HeaderOperationKind::Add => {
                    let value = op
                        .value
                        .as_deref()
                        .map(|template| expand_value_template(template, &ctx))
                        .unwrap_or_default();
                    headers.push((name, value));
                }
                HeaderOperationKind::Remove => {
                    headers.retain(|(key, _)| key != &name);
                }
            }
        }

        ctx.response_headers = headers.clone();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(
        request_headers: &[(&str, &str)],
        response_headers: &[(&str, &str)],
        status: u16,
        url: &str,
    ) -> RuleContext {
        RuleContext {
            method: "GET".to_string(),
            url: url::Url::parse(url).unwrap(),
            request_headers: request_headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            response_status: status,
            response_headers: response_headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn eval(src: &str, context: &RuleContext) -> bool {
        parse_expression(src).unwrap().eval(context)
    }

    #[test]
    fn test_status_and_content_type_comparison() {
        let context = ctx(
            &[],
            &[("content-type", "application/json; charset=utf-8")],
            200,
            "https://example.com/api",
        );
        assert!(eval("http.response.status == 200", &context));
        assert!(eval("http.response.status != 404", &context));
        assert!(!eval("http.response.status == 404", &context));
        assert!(eval(
            "starts_with(http.response.content_type, \"application/json\")",
            &context
        ));
    }

    #[test]
    fn test_boolean_combinators_and_parens() {
        let context = ctx(
            &[],
            &[("content-type", "text/css")],
            200,
            "https://example.com/a.css",
        );
        assert!(eval(
            "http.response.status == 200 and ends_with(url.path, \".css\")",
            &context
        ));
        assert!(eval(
            "http.response.status == 500 or ends_with(url.path, \".css\")",
            &context
        ));
        assert!(eval(
            "not (http.response.status == 500) and http.response.status == 200",
            &context
        ));
        assert!(!eval(
            "http.response.status == 200 and http.response.status == 500",
            &context
        ));
    }

    #[test]
    fn test_eq_ne_and_in_operators() {
        let context = ctx(
            &[("x-client", "ios")],
            &[("content-type", "application/json")],
            200,
            "https://example.com/api",
        );
        // `eq`/`ne` son la sintaxis canónica de Cloudflare; `==`/`!=` funcionan como alias.
        assert!(eval("http.response.status eq 200", &context));
        assert!(eval("http.response.status ne 404", &context));
        assert!(eval("http.response.status == 200", &context));
        // Conjuntos `in { ... }` (miembros separados por espacios, sin comas).
        assert!(eval("http.response.status in {200 201 204}", &context));
        assert!(!eval("http.response.status in {301 302}", &context));
        assert!(eval(
            "http.request.headers[\"x-client\"] in {\"ios\" \"android\"}",
            &context
        ));
        // El tipo de cada miembro tiene que coincidir con el del campo.
        assert!(parse_expression("http.response.status in {200 \"x\"}").is_err());
        assert!(parse_expression("http.response.status in {}").is_err());
        // Campo ausente: `in` tampoco matchea.
        let empty = ctx(&[], &[], 200, "https://example.com/");
        assert!(!eval("http.request.headers[\"x-nada\"] in {\"a\"}", &empty));
    }

    #[test]
    fn test_header_access_missing_is_false_not_error() {
        let context = ctx(&[], &[], 200, "https://example.com/");
        // El header no existe: la comparación es falsa (semántica wirefilter), no un error.
        assert!(!eval(
            "http.response.headers[\"x-missing\"] == \"y\"",
            &context
        ));
        assert!(!eval(
            "contains(http.response.headers[\"x-missing\"], \"y\")",
            &context
        ));
        // `!=` contra un campo ausente tampoco matchea: Null no es distinto de "y", es ausente.
        assert!(!eval(
            "http.response.headers[\"x-missing\"] != \"y\"",
            &context
        ));
    }

    #[test]
    fn test_request_headers_and_url_fields() {
        let context = ctx(
            &[("x-client", "ios")],
            &[],
            200,
            "https://cdn.example.com/img/photo.jpg?v=2",
        );
        assert!(eval(
            "http.request.headers[\"x-client\"] == \"ios\"",
            &context
        ));
        assert!(eval("http.request.method == \"GET\"", &context));
        assert!(eval("url.host == \"cdn.example.com\"", &context));
        assert!(eval("url.scheme == \"https\"", &context));
        assert!(eval("url.path == \"/img/photo.jpg\"", &context));
        assert!(eval("contains(url.query, \"v=2\")", &context));
        assert!(eval("matches(url.path, \"\\\\.jpg$\")", &context));
    }

    #[test]
    fn test_string_functions() {
        let context = ctx(&[], &[], 200, "https://example.com/ABC");
        assert!(eval("lower(url.path) == \"/abc\"", &context));
        assert!(eval("upper(url.path) == \"/ABC\"", &context));
        assert!(eval("len(url.host) == 11", &context));
        assert!(eval(
            "concat(\"a\", \"-\", url.host) == \"a-example.com\"",
            &context
        ));
    }

    #[test]
    fn test_unknown_field_and_function_rejected() {
        assert!(parse_expression("http.response.bogus == 1").is_err());
        assert!(parse_expression("cf.colo == \"mad\"").is_err());
        assert!(parse_expression("unknown_fn(url.path, \"x\")").is_err());
        assert!(parse_expression("len(200) == 3").is_err());
        assert!(parse_expression("http.response.status == \"200\"").is_err());
        assert!(parse_expression("http.response.status == 200 and").is_err());
        assert!(parse_expression("").is_err());
        assert!(validate_expression("matches(url.path, \"[unclosed\")").is_err());
        assert!(validate_expression("matches(url.path, \"\\\\.jpg$\")").is_ok());
    }

    #[test]
    fn test_case_insensitive_field_error_message_lists_path() {
        let err = parse_expression("http.request.headerz[\"x\"] == \"y\"").unwrap_err();
        assert!(err.contains("http.request.headerz"), "{err}");
    }

    #[test]
    fn test_apply_set_add_remove() {
        let mut headers = vec![
            ("content-type".to_string(), "text/html".to_string()),
            ("server".to_string(), "nginx".to_string()),
        ];
        let rules = vec![
            HeaderRule {
                expression: "http.response.status == 200".to_string(),
                action: HeaderRuleAction::Set,
                action_parameters: HeaderActionParameters {
                    headers: vec![
                        HeaderOperation {
                            name: "x-algo".to_string(),
                            operation: HeaderOperationKind::Set,
                            value: Some("asi".to_string()),
                        },
                        HeaderOperation {
                            name: "server".to_string(),
                            operation: HeaderOperationKind::Remove,
                            value: None,
                        },
                    ],
                },
            },
            HeaderRule {
                expression: "ends_with(url.path, \".json\")".to_string(),
                action: HeaderRuleAction::Set,
                action_parameters: HeaderActionParameters {
                    headers: vec![HeaderOperation {
                        name: "set-cookie".to_string(),
                        operation: HeaderOperationKind::Add,
                        value: Some("a=1".to_string()),
                    }],
                },
            },
        ];
        let context = ctx(&[], &[], 200, "https://example.com/data.json");

        apply_header_rules(&rules, &context, &mut headers);

        assert!(headers.contains(&("x-algo".to_string(), "asi".to_string())));
        assert!(!headers.iter().any(|(k, _)| k == "server"));
        assert!(headers.contains(&("set-cookie".to_string(), "a=1".to_string())));
    }

    #[test]
    fn test_apply_rules_see_previous_mutations_in_order() {
        let mut headers: Vec<(String, String)> = Vec::new();
        let rules = vec![
            HeaderRule {
                expression: "true == true".to_string(),
                action: HeaderRuleAction::Set,
                action_parameters: HeaderActionParameters {
                    headers: vec![HeaderOperation {
                        name: "x-step".to_string(),
                        operation: HeaderOperationKind::Set,
                        value: Some("one".to_string()),
                    }],
                },
            },
            // La segunda regla observa el header que puso la primera.
            HeaderRule {
                expression: "http.response.headers[\"x-step\"] == \"one\"".to_string(),
                action: HeaderRuleAction::Set,
                action_parameters: HeaderActionParameters {
                    headers: vec![HeaderOperation {
                        name: "x-step".to_string(),
                        operation: HeaderOperationKind::Set,
                        value: Some("two".to_string()),
                    }],
                },
            },
        ];
        let context = ctx(&[], &[], 200, "https://example.com/");

        apply_header_rules(&rules, &context, &mut headers);

        assert_eq!(headers, vec![("x-step".to_string(), "two".to_string())]);
    }

    #[test]
    fn test_apply_set_replaces_all_previous_values() {
        let mut headers = vec![
            ("x-multi".to_string(), "a".to_string()),
            ("x-multi".to_string(), "b".to_string()),
        ];
        let rules = vec![HeaderRule {
            expression: "true == true".to_string(),
            action: HeaderRuleAction::Set,
            action_parameters: HeaderActionParameters {
                headers: vec![HeaderOperation {
                    name: "x-multi".to_string(),
                    operation: HeaderOperationKind::Set,
                    value: Some("c".to_string()),
                }],
            },
        }];
        let context = ctx(&[], &[], 200, "https://example.com/");

        apply_header_rules(&rules, &context, &mut headers);

        assert_eq!(headers, vec![("x-multi".to_string(), "c".to_string())]);
    }

    #[test]
    fn test_value_template_expansion() {
        let context = ctx(
            &[("x-client", "ios")],
            &[("content-type", "application/json")],
            201,
            "https://example.com/api",
        );

        assert_eq!(
            expand_value_template("status=${http.response.status};host=${url.host}", &context),
            "status=201;host=example.com"
        );
        assert_eq!(
            expand_value_template("client=${http.request.headers[\"x-client\"]}", &context),
            "client=ios"
        );
        // Campo ausente → vacío; placeholder roto → se preserva literal.
        assert_eq!(
            expand_value_template("[${http.request.headers[\"x-nada\"]}]", &context),
            "[]"
        );
        assert_eq!(
            expand_value_template("${no-cierre", &context),
            "${no-cierre"
        );
    }

    #[test]
    fn test_value_template_validation() {
        assert!(validate_value_template("${url.host}").is_ok());
        assert!(validate_value_template("${http.response.headers[\"x-a\"]}").is_ok());
        assert!(validate_value_template("${url.bogus}").is_err());
        assert!(validate_value_template("${sin-cierre").is_err());
    }
}
