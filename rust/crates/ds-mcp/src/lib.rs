//! Dunking Sheep MCP server (stdio transport), ported from `dunk_mcp.py` (T45).
//!
//! Speaks JSON-RPC 2.0 over newline-delimited stdin/stdout, as the Model Context Protocol's
//! stdio transport requires. Every command in the registry marked `mcp` is reachable as a tool
//! (through its group's `action`, or by its flat name); calls are forwarded to the daemon over
//! its unix socket (starting it if needed).
//!
//! Register with Claude Code: `claude mcp add --scope user dunkingsheep -- dunkingsheep mcp`.
//! Because Claude Code spawns this process inside its own pane, it inherits the pane id, so
//! the agent can target "self".

#![forbid(unsafe_code)]

pub mod pyjson;

use std::io::{BufRead, Write};

use ds_core::client::{ClientError, FlockClient, Transport};
use ds_core::registry::{self, Group};
use ds_core::roles;
use serde_json::{json, Map, Value};

use pyjson::{dumps, py_falsy, py_repr, py_type_name};

pub const SUPPORTED_PROTOCOLS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// `structuredContent` was introduced in 2025-06-18; older clients get text only.
pub const STRUCTURED_CONTENT_SINCE: &str = "2025-06-18";
/// `dunk_core.VERSION`: ds-core has no version constant yet, so the server pins its own; the
/// test `version_matches_the_python_while_it_exists` keeps the two in step until cutover.
pub const SERVER_VERSION: &str = "2.71.4";

/// The `instructions` text, byte-for-byte `dunk_mcp.INSTRUCTIONS` (a test pins its SHA-256).
pub const INSTRUCTIONS: &str = include_str!("instructions.txt");

/// Whatever can run a daemon command: a [`FlockClient`] in production, a fake in tests.
pub trait Caller {
    fn call(&mut self, cmd: &str, args: Map<String, Value>) -> Result<Value, ClientError>;
}

impl<T: Transport> Caller for FlockClient<T> {
    fn call(&mut self, cmd: &str, args: Map<String, Value>) -> Result<Value, ClientError> {
        FlockClient::call(self, cmd, args)
    }
}

/// Builds a connected client; the Python calls its `client_factory` once per request.
pub type ClientFactory = Box<dyn FnMut() -> Result<Box<dyn Caller>, ClientError>>;

pub struct McpServer {
    factory: ClientFactory,
    pub initialized: bool,
    pub protocol_version: String,
    role_cache: Option<String>,
}

impl McpServer {
    pub fn new(factory: ClientFactory) -> Self {
        McpServer {
            factory,
            initialized: false,
            protocol_version: SUPPORTED_PROTOCOLS[0].to_string(),
            role_cache: None,
        }
    }

    // -- transport ------------------------------------------------------------------------

    /// `serve_forever`: a closed pipe ends the loop quietly (the harness went away).
    pub fn serve_forever(
        &mut self,
        input: &mut impl BufRead,
        output: &mut impl Write,
    ) -> std::io::Result<()> {
        match self.serve_loop(input, output) {
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            other => other,
        }
    }

    fn serve_loop(
        &mut self,
        input: &mut impl BufRead,
        output: &mut impl Write,
    ) -> std::io::Result<()> {
        let mut raw = Vec::new();
        loop {
            raw.clear();
            if input.read_until(b'\n', &mut raw)? == 0 {
                return Ok(());
            }
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let message: Value = match serde_json::from_str(line) {
                Ok(message) => message,
                Err(_) => {
                    write_message(
                        output,
                        &json!({"jsonrpc": "2.0", "id": null,
                                "error": {"code": -32700, "message": "parse error"}}),
                    )?;
                    continue;
                }
            };
            let messages = match message {
                Value::Array(items) => items,
                single => vec![single],
            };
            for item in &messages {
                if let Some(response) = self.handle(item) {
                    write_message(output, &response)?;
                }
            }
        }
    }

    // -- dispatch -------------------------------------------------------------------------

    /// Return a response, or None for notifications.
    pub fn handle(&mut self, message: &Value) -> Option<Value> {
        let Some(object) = message.as_object() else {
            return Some(error_response(&Value::Null, -32600, "invalid request"));
        };
        let msg_id = object.get("id").cloned().unwrap_or(Value::Null);
        let is_notification = !object.contains_key("id");
        let method = match object.get("method") {
            None | Some(Value::Null) => {
                // A response to something we sent (we never send requests) - ignore.
                return None;
            }
            Some(method) => method,
        };
        let raw_params = object.get("params").cloned().unwrap_or(Value::Null);
        let params = if py_falsy(&raw_params) {
            json!({})
        } else {
            raw_params
        };

        let outcome = self.dispatch(method, &params);
        let result = match outcome {
            Ok(Dispatch::Notification) => return None,
            Ok(Dispatch::UnknownMethod(name)) => {
                if is_notification {
                    return None;
                }
                return Some(error_response(
                    &msg_id,
                    -32601,
                    &format!("method not found: {name}"),
                ));
            }
            Ok(Dispatch::Result(result)) => result,
            Err(text) => {
                eprintln!("handler for {} failed: {text}", display_method(method));
                if is_notification {
                    return None;
                }
                return Some(error_response(
                    &msg_id,
                    -32603,
                    &format!("internal error: {text}"),
                ));
            }
        };
        if is_notification {
            return None;
        }
        Some(json!({"jsonrpc": "2.0", "id": msg_id, "result": result}))
    }

    fn dispatch(&mut self, method: &Value, params: &Value) -> Result<Dispatch, String> {
        let name = method.as_str();
        if name == Some("initialize") {
            return Ok(Dispatch::Result(self.initialize(params)?));
        }
        if name == Some("notifications/initialized") {
            self.initialized = true;
            return Ok(Dispatch::Notification);
        }
        // `method.startswith(...)`: a non-string method is an AttributeError in the Python.
        let Some(name) = name else {
            return Err(format!(
                "'{}' object has no attribute 'startswith'",
                py_type_name(method)
            ));
        };
        Ok(match name {
            _ if name.starts_with("notifications/") => Dispatch::Notification,
            "ping" => Dispatch::Result(json!({})),
            "tools/list" => Dispatch::Result(json!({"tools": self.list_tools()})),
            "tools/call" => {
                let tool = param(params, "name")?;
                let arguments = param(params, "arguments")?;
                let arguments = if py_falsy(&arguments) {
                    json!({})
                } else {
                    arguments
                };
                Dispatch::Result(self.call_tool(&tool, &arguments)?)
            }
            "resources/list" => Dispatch::Result(json!({"resources": []})),
            "resources/templates/list" => Dispatch::Result(json!({"resourceTemplates": []})),
            "prompts/list" => Dispatch::Result(json!({"prompts": []})),
            other => Dispatch::UnknownMethod(other.to_string()),
        })
    }

    // -- methods --------------------------------------------------------------------------

    fn initialize(&mut self, params: &Value) -> Result<Value, String> {
        let requested = param(params, "protocolVersion")?;
        let version = match requested.as_str() {
            Some(text) if SUPPORTED_PROTOCOLS.contains(&text) => text,
            _ => SUPPORTED_PROTOCOLS[0],
        };
        self.protocol_version = version.to_string();
        Ok(json!({
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "dunkingsheep", "version": SERVER_VERSION},
            "instructions": INSTRUCTIONS,
        }))
    }

    /// This process's role, asked once: the list of tools follows it.
    fn role(&mut self) -> String {
        if let Some(cached) = &self.role_cache {
            return cached.clone();
        }
        // An unreachable daemon lists the read-only tools (role "anonymous").
        let role = (self.factory)()
            .and_then(|mut client| client.call("whoami", Map::new()))
            .ok()
            .and_then(|reply| {
                reply
                    .get("role")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|role| !role.is_empty())
            .unwrap_or_else(|| "anonymous".to_string());
        self.role_cache = Some(role.clone());
        role
    }

    pub fn list_tools(&mut self) -> Vec<Value> {
        let role = self.role();
        registry::GROUPS
            .iter()
            .filter(|group| {
                group
                    .actions
                    .iter()
                    .any(|(_, command)| roles::allowed(&role, command))
            })
            .map(|group| {
                json!({
                    "name": group.name,
                    "description": group.description,
                    "inputSchema": registry::grouped_input_schema(group.name)
                        .expect("every group has a schema"),
                })
            })
            .collect()
    }

    /// `call_tool`: an `Err` is an internal error (-32603); every refusal is an `isError` result.
    pub fn call_tool(&mut self, name: &Value, arguments: &Value) -> Result<Value, String> {
        let Some(arguments) = arguments.as_object() else {
            return Ok(tool_error("arguments must be an object"));
        };
        let mut arguments = arguments.clone();
        let group: Option<&Group> = name
            .as_str()
            .and_then(|text| registry::GROUPS.iter().find(|group| group.name == text));
        let command_name: Option<String> = match group {
            Some(group) => {
                let action = arguments.remove("action").unwrap_or(Value::Null);
                match resolve(group, &action)? {
                    Some(command) => Some(command.to_string()),
                    None => {
                        let expected: Vec<&str> =
                            group.actions.iter().map(|(action, _)| *action).collect();
                        return Ok(tool_error(&format!(
                            "unknown action {} for {}; expected one of {}",
                            py_repr(&action),
                            group.name,
                            expected.join(", ")
                        )));
                    }
                }
            }
            // Flat command names keep working: the merge changed the surface, not the
            // capabilities.
            None => name.as_str().map(str::to_string),
        };
        let command = command_name.as_deref().and_then(registry::command);
        let Some(command) = command.filter(|command| command.mcp) else {
            return Ok(tool_error(&format!("unknown tool {}", py_repr(name))));
        };
        let missing: Vec<&str> = command
            .required
            .iter()
            .copied()
            .filter(|key| arguments.get(*key).map_or(true, Value::is_null))
            .collect();
        if !missing.is_empty() {
            return Ok(tool_error(&format!(
                "{}: missing required argument(s): {}",
                display_name(name),
                missing.join(", ")
            )));
        }
        let outcome = (self.factory)().and_then(|mut client| client.call(command.name, arguments));
        let result = match outcome {
            Ok(result) => result,
            Err(ClientError::Dunk(error)) => return Ok(tool_error(&error.to_string())),
            Err(error @ (ClientError::Unavailable(_) | ClientError::Io(_))) => {
                return Ok(tool_error(&format!(
                    "dunkingsheep daemon unavailable: {error}"
                )));
            }
            // A reply that was not JSON is a ValueError in the Python: it is not caught there,
            // so it surfaces as an internal error.
            Err(ClientError::Protocol(text)) => return Err(text),
        };
        let text = match &result {
            Value::String(text) => text.clone(),
            other => dumps(other, Some(2)),
        };
        let mut payload = json!({"content": [{"type": "text", "text": text}], "isError": false});
        if result.is_object() && self.protocol_version.as_str() >= STRUCTURED_CONTENT_SINCE {
            payload["structuredContent"] = result;
        }
        Ok(payload)
    }
}

enum Dispatch {
    Result(Value),
    Notification,
    UnknownMethod(String),
}

/// `params.get(key)`: params that are not an object are an AttributeError in the Python.
fn param(params: &Value, key: &str) -> Result<Value, String> {
    match params.as_object() {
        Some(object) => Ok(object.get(key).cloned().unwrap_or(Value::Null)),
        None => Err(format!(
            "'{}' object has no attribute 'get'",
            py_type_name(params)
        )),
    }
}

/// `GroupedTool.resolve`: `dict.get(action)`; an unhashable action is a TypeError there.
fn resolve(group: &Group, action: &Value) -> Result<Option<&'static str>, String> {
    match action {
        Value::Array(_) => Err("unhashable type: 'list'".to_string()),
        Value::Object(_) => Err("unhashable type: 'dict'".to_string()),
        Value::String(text) => Ok(group
            .actions
            .iter()
            .find(|(candidate, _)| candidate == text)
            .map(|(_, command)| *command)),
        _ => Ok(None),
    }
}

/// `f"{name}"`: the tool name as the Python interpolates it (a string prints bare).
fn display_name(name: &Value) -> String {
    match name {
        Value::String(text) => text.clone(),
        Value::Null => "None".to_string(),
        other => py_repr(other),
    }
}

fn display_method(method: &Value) -> String {
    display_name(method)
}

fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn tool_error(message: &str) -> Value {
    json!({"content": [{"type": "text", "text": message}], "isError": true})
}

/// One line out. The envelope keys go in the Python's order (`jsonrpc`, `id`, then `result` or
/// `error`) because `serde_json` would sort them; inside `result` the order is the map's.
fn write_message(output: &mut impl Write, payload: &Value) -> std::io::Result<()> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(object) = payload.as_object() {
        for key in ["jsonrpc", "id", "result", "error"] {
            if let Some(value) = object.get(key) {
                parts.push(format!(
                    "{}: {}",
                    dumps(&json!(key), None),
                    dumps(value, None)
                ));
            }
        }
    }
    output.write_all(format!("{{{}}}\n", parts.join(", ")).as_bytes())?;
    output.flush()
}
