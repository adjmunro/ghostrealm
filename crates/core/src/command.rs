//! Command metadata, argument specs, and typed argument access.
//!
//! A command is described by [`CommandMeta`] (id/title/description + an argument
//! schema). The same metadata drives the palette, keybindings, and the agent
//! channel, so it must be self-describing enough for an LLM to call `set(id,
//! args)` from the schema alone.

use std::collections::HashMap;
use std::fmt;

/// The kind of a single argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgKind {
    Str,
    Int,
    Bool,
    /// One of a fixed set of string values.
    Enum(Vec<String>),
}

impl fmt::Display for ArgKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArgKind::Str => write!(f, "string"),
            ArgKind::Int => write!(f, "int"),
            ArgKind::Bool => write!(f, "bool"),
            ArgKind::Enum(vs) => write!(f, "enum({})", vs.join("|")),
        }
    }
}

/// One argument in a command's schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArgSpec {
    pub name: &'static str,
    pub kind: ArgKind,
    pub required: bool,
    pub description: &'static str,
}

impl ArgSpec {
    pub fn new(
        name: &'static str,
        kind: ArgKind,
        required: bool,
        description: &'static str,
    ) -> Self {
        ArgSpec {
            name,
            kind,
            required,
            description,
        }
    }
    pub fn required(name: &'static str, kind: ArgKind, description: &'static str) -> Self {
        Self::new(name, kind, true, description)
    }
    pub fn optional(name: &'static str, kind: ArgKind, description: &'static str) -> Self {
        Self::new(name, kind, false, description)
    }
}

/// Self-describing metadata for one command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandMeta {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub args: Vec<ArgSpec>,
}

impl CommandMeta {
    pub fn new(id: &'static str, title: &'static str, description: &'static str) -> Self {
        CommandMeta {
            id,
            title,
            description,
            args: Vec::new(),
        }
    }
    pub fn arg(mut self, spec: ArgSpec) -> Self {
        self.args.push(spec);
        self
    }
}

/// A concrete argument value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl Value {
    fn kind_name(&self) -> &'static str {
        match self {
            Value::Str(_) => "string",
            Value::Int(_) => "int",
            Value::Bool(_) => "bool",
        }
    }
}

/// Errors from validating or reading arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgError {
    Missing(String),
    WrongType {
        name: String,
        expected: &'static str,
        got: &'static str,
    },
    NotInEnum {
        name: String,
        got: String,
        allowed: Vec<String>,
    },
    Unknown(String),
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArgError::Missing(n) => write!(f, "missing required argument `{n}`"),
            ArgError::WrongType {
                name,
                expected,
                got,
            } => {
                write!(f, "argument `{name}` should be {expected}, got {got}")
            }
            ArgError::NotInEnum { name, got, allowed } => {
                write!(
                    f,
                    "argument `{name}` = `{got}` not in [{}]",
                    allowed.join(", ")
                )
            }
            ArgError::Unknown(n) => write!(f, "unknown argument `{n}`"),
        }
    }
}

impl std::error::Error for ArgError {}

/// A bag of argument values keyed by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    map: HashMap<String, Value>,
}

impl Args {
    pub fn new() -> Self {
        Args::default()
    }

    pub fn with(mut self, name: &str, value: Value) -> Self {
        self.map.insert(name.to_string(), value);
        self
    }

    pub fn insert(&mut self, name: &str, value: Value) {
        self.map.insert(name.to_string(), value);
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.map.get(name)
    }

    pub fn get_str(&self, name: &str) -> Result<&str, ArgError> {
        match self.map.get(name) {
            Some(Value::Str(s)) => Ok(s),
            Some(v) => Err(ArgError::WrongType {
                name: name.into(),
                expected: "string",
                got: v.kind_name(),
            }),
            None => Err(ArgError::Missing(name.into())),
        }
    }

    pub fn opt_str(&self, name: &str) -> Option<&str> {
        match self.map.get(name) {
            Some(Value::Str(s)) => Some(s),
            _ => None,
        }
    }

    pub fn get_int(&self, name: &str) -> Result<i64, ArgError> {
        match self.map.get(name) {
            Some(Value::Int(i)) => Ok(*i),
            Some(v) => Err(ArgError::WrongType {
                name: name.into(),
                expected: "int",
                got: v.kind_name(),
            }),
            None => Err(ArgError::Missing(name.into())),
        }
    }

    pub fn get_bool(&self, name: &str) -> Result<bool, ArgError> {
        match self.map.get(name) {
            Some(Value::Bool(b)) => Ok(*b),
            Some(v) => Err(ArgError::WrongType {
                name: name.into(),
                expected: "bool",
                got: v.kind_name(),
            }),
            None => Err(ArgError::Missing(name.into())),
        }
    }

    /// Validate this argument bag against a schema: required args present, types
    /// match, enum values allowed, and no unknown args.
    pub fn validate(&self, schema: &[ArgSpec]) -> Result<(), ArgError> {
        for spec in schema {
            match self.map.get(spec.name) {
                None => {
                    if spec.required {
                        return Err(ArgError::Missing(spec.name.into()));
                    }
                }
                Some(value) => check_kind(spec, value)?,
            }
        }
        for key in self.map.keys() {
            if !schema.iter().any(|s| s.name == key) {
                return Err(ArgError::Unknown(key.clone()));
            }
        }
        Ok(())
    }
}

fn check_kind(spec: &ArgSpec, value: &Value) -> Result<(), ArgError> {
    let ok = match (&spec.kind, value) {
        (ArgKind::Str, Value::Str(_)) => true,
        (ArgKind::Int, Value::Int(_)) => true,
        (ArgKind::Bool, Value::Bool(_)) => true,
        (ArgKind::Enum(allowed), Value::Str(s)) => {
            if !allowed.iter().any(|a| a == s) {
                return Err(ArgError::NotInEnum {
                    name: spec.name.into(),
                    got: s.clone(),
                    allowed: allowed.clone(),
                });
            }
            true
        }
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(ArgError::WrongType {
            name: spec.name.into(),
            expected: match spec.kind {
                ArgKind::Str | ArgKind::Enum(_) => "string",
                ArgKind::Int => "int",
                ArgKind::Bool => "bool",
            },
            got: value.kind_name(),
        })
    }
}
