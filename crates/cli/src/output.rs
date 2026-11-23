//! Text / JSON output (SPEC §4.3).

use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};

use migrate_db_core::error::Error;

use crate::cli::{GlobalArgs, OutputFormat};

/// JSON output schema version.
pub const SCHEMA: u32 = 1;

/// Where results go.
#[derive(Debug, Clone)]
pub struct Output {
    format: OutputFormat,
    file: Option<PathBuf>,
}

impl Output {
    /// From global flags.
    pub fn new(global: &GlobalArgs) -> Self {
        Self {
            format: global.output,
            file: global.output_file.clone(),
        }
    }

    /// `true` in JSON mode.
    pub fn is_json(&self) -> bool {
        self.format == OutputFormat::Json
    }

    /// Emit a command result. `body` is merged into `{schema, command}`.
    pub fn emit(
        &self,
        command: &str,
        body: Value,
        text: impl FnOnce() -> String,
    ) -> Result<(), Error> {
        let doc = document(command, body);
        if let Some(path) = &self.file {
            let pretty = serde_json::to_string_pretty(&doc)
                .map_err(|e| Error::Usage(format!("cannot serialize output: {e}")))?;
            fs::write(path, pretty + "\n").map_err(|e| {
                Error::Usage(format!(
                    "cannot write --output-file `{}`: {e}",
                    path.display()
                ))
            })?;
        }
        match self.format {
            OutputFormat::Json => println!("{doc}"),
            OutputFormat::Text => {
                let t = text();
                if !t.is_empty() {
                    println!("{}", t.trim_end());
                }
            }
        }
        Ok(())
    }

    /// Report an error (stdout JSON document, or stderr text).
    pub fn error(&self, command: &str, error: &Error) {
        match self.format {
            OutputFormat::Json => {
                let doc = document(
                    command,
                    json!({ "error": { "message": error.to_string(), "exit_code": error.exit_code() } }),
                );
                println!("{doc}");
            }
            OutputFormat::Text => eprintln!("error: {error}"),
        }
    }
}

fn document(command: &str, body: Value) -> Value {
    let mut doc = json!({ "schema": SCHEMA, "command": command });
    if let (Value::Object(target), Value::Object(extra)) = (&mut doc, body) {
        target.extend(extra);
    }
    doc
}

/// Serialize anything serializable, falling back to `null`.
pub fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}
