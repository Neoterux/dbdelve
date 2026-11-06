//! `dbdelve mcp`: the process Claude Code starts, speaking MCP on its stdio.
//!
//! This is the only file that names rmcp or tokio's sockets. It answers
//! `initialize` and `server/discover` itself, answers `tools/list` from the
//! static table in [`super::tools`] without connecting anywhere, and carries
//! each `tools/call` to the app over one connection of its own. The shim never
//! opens a database connection and answers nothing but `app_not_running` on its
//! own: every other status is the app's. A call that is cancelled is cancelled
//! by hanging up its connection, and no frame says so.
//!
//! Nothing here is remembered between calls about the app: whether it is
//! running is decided at each call, by whether its socket takes a connection.

#[cfg(unix)]
use std::path::PathBuf;

#[cfg(unix)]
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
    transport::stdio,
};
// The library's `Tool` is spelled the same as the agent's, which is the one
// this file means by `Tool`.
#[cfg(unix)]
use rmcp::model::Tool as McpTool;
#[cfg(unix)]
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

#[cfg(unix)]
use super::{Reply, Status, tools::Tool, wire};

/// What an agent is told when it cannot reach the app. The shim cannot tell an
/// app that is closed from one with its agent access off, so it says both.
#[cfg(unix)]
const NOT_RUNNING: &str = "DBDelve is not running, or its agent access is off.";

/// What the shim serves.
#[cfg(unix)]
pub(crate) struct Shim {
    /// Where the app's socket is, or why that could not be worked out. Taken
    /// as it comes: a path that cannot be derived is the answer to every call,
    /// not a reason for the shim not to speak.
    socket: Result<PathBuf, String>,
}

#[cfg(unix)]
impl Shim {
    pub(crate) fn new(socket: Result<PathBuf, String>) -> Self {
        Self { socket }
    }

    /// Carry one call to the app and bring back its answer.
    async fn carry(&self, tool: Tool, connection: Option<String>) -> CallToolResult {
        let path = match &self.socket {
            Ok(path) => path,
            Err(text) => return answer(Status::AppNotRunning, text),
        };
        let mut stream = match UnixStream::connect(path).await {
            Ok(stream) => stream,
            Err(error) => return answer(Status::AppNotRunning, &failed_connect_text(&error)),
        };
        let hello = wire::encode(&wire::Hello::Call {
            v: wire::PROTOCOL_VERSION,
            tool,
            connection,
        });
        if stream.write_all(hello.as_bytes()).await.is_err() {
            return closed_without_answering();
        }
        match read_reply(&mut stream).await {
            Some(reply) => answer(reply.status, &reply.text),
            None => closed_without_answering(),
        }
    }
}

#[cfg(unix)]
impl ServerHandler for Shim {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build());
        // The struct cannot be built by literal from here, and its default
        // name would announce the library rather than this program.
        info.server_info = Implementation::new("dbdelve", env!("CARGO_PKG_VERSION"));
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(
            Tool::ALL
                .into_iter()
                .map(|tool| McpTool::new(tool.name(), tool.description(), tool.input_schema()))
                .collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let (tool, connection) = resolve(&request)?;
        Ok(self.carry(tool, connection).await.into())
    }
}

/// The tool a request names and the connection it gives, or why the request is
/// not one. An unknown tool or an argument the tool does not take is a protocol
/// error answered here, not a tool result, and it never reaches the app.
#[cfg(unix)]
fn resolve(request: &CallToolRequestParams) -> Result<(Tool, Option<String>), ErrorData> {
    let tool = Tool::from_name(&request.name).ok_or_else(|| {
        ErrorData::invalid_params(format!("There is no tool named `{}`.", request.name), None)
    })?;
    let none = JsonObject::new();
    let arguments = request.arguments.as_ref().unwrap_or(&none);
    match tool {
        Tool::ListConnections if arguments.is_empty() => Ok((tool, None)),
        Tool::ListConnections => Err(ErrorData::invalid_params(
            "`list_connections` takes no arguments.",
            None,
        )),
        Tool::Ping => match (arguments.len(), arguments.get("connection")) {
            (1, Some(serde_json::Value::String(name))) => Ok((tool, Some(name.clone()))),
            _ => Err(ErrorData::invalid_params(
                "`ping` takes one string argument, `connection`.",
                None,
            )),
        },
    }
}

/// A tool's result: its status first, then one sentence or the app's own text.
/// Only `executed` is not an error.
#[cfg(unix)]
fn answer(status: Status, text: &str) -> CallToolResult {
    let content = vec![ContentBlock::text(format!(
        "status: {}\n{text}",
        status.as_str()
    ))];
    if status == Status::Executed {
        CallToolResult::success(content)
    } else {
        CallToolResult::error(content)
    }
}

/// The connection ended with no answer on it, which is all the shim saw.
#[cfg(unix)]
fn closed_without_answering() -> CallToolResult {
    answer(
        Status::AppNotRunning,
        "DBDelve closed the connection without answering.",
    )
}

/// What a failed connect is reported as: the app's absence in the two ways a
/// socket path can say it, and for any other failure only what was observed.
#[cfg(unix)]
fn failed_connect_text(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
            NOT_RUNNING.to_string()
        }
        std::io::ErrorKind::PermissionDenied => {
            "Could not connect to DBDelve's agent socket: permission denied.".to_string()
        }
        _ => format!("Could not connect to DBDelve's agent socket: {error}."),
    }
}

/// The one answer line a call gets, read without holding the runtime: a line
/// is at most the frame limit, and one that does not end in a newline, or does
/// not decode, is no answer.
#[cfg(unix)]
async fn read_reply(stream: &mut UnixStream) -> Option<Reply> {
    let mut line = Vec::new();
    let mut reader = BufReader::new(stream.take(wire::FRAME_LIMIT));
    reader.read_until(b'\n', &mut line).await.ok()?;
    if line.pop() != Some(b'\n') {
        return None;
    }
    serde_json::from_slice(&line).ok()
}

/// Serve MCP on stdio until the client closes it. The exit code is the
/// process's.
#[cfg(unix)]
pub(crate) fn run() -> i32 {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("The agent shim could not start its runtime: {error}.");
            return 1;
        }
    };
    runtime.block_on(async {
        let service = match Shim::new(super::socket_path()).serve(stdio()).await {
            Ok(service) => service,
            Err(error) => {
                eprintln!("The agent shim could not start serving: {error}.");
                return 1;
            }
        };
        match service.waiting().await {
            Ok(_) => 0,
            Err(error) => {
                eprintln!("The agent shim stopped serving: {error}.");
                1
            }
        }
    })
}

/// Release Windows builds have no console, so the exit code is the contract.
#[cfg(not(unix))]
pub(crate) fn run() -> i32 {
    eprintln!("{}", crate::agent::UNAVAILABLE);
    1
}

#[cfg(all(test, unix))]
mod tests {
    use std::{future::Future, os::unix::net::UnixListener, sync::mpsc, thread};

    use serde_json::{Value, json};
    use tokio::{
        io::{DuplexStream, Lines, ReadHalf, WriteHalf, split},
        time::{Duration, timeout},
    };

    use super::*;
    use crate::agent::{Listener, tests::short_socket_path};

    /// Longer than anything here takes, so a wrong method name or a missing
    /// field fails the test instead of hanging it.
    const PATIENCE: Duration = Duration::from_secs(5);

    fn block_on<T>(test: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime builds")
            .block_on(test)
    }

    /// The client end of a duplex whose other end the shim serves, speaking raw
    /// JSON-RPC lines: rmcp's client side is not built.
    struct Client {
        lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
        writer: WriteHalf<DuplexStream>,
        next_id: u64,
    }

    impl Client {
        /// Serving does not return until the first message arrives, so the
        /// shim runs as its own task rather than before the client speaks.
        fn serve(shim: Shim) -> Self {
            let (client, server) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                if let Ok(service) = shim.serve(server).await {
                    let _ = service.waiting().await;
                }
            });
            let (read, writer) = split(client);
            Self {
                lines: BufReader::new(read).lines(),
                writer,
                next_id: 1,
            }
        }

        async fn send(&mut self, message: Value) {
            self.writer
                .write_all(format!("{message}\n").as_bytes())
                .await
                .expect("the shim takes the line");
        }

        async fn line(&mut self) -> Value {
            let line = timeout(PATIENCE, self.lines.next_line())
                .await
                .expect("the shim writes a line in time")
                .expect("the line reads")
                .expect("the shim has not closed its end");
            serde_json::from_str(&line).expect("the shim writes JSON")
        }

        /// The response to a new request, passing over anything the shim writes
        /// that is not it.
        async fn request(&mut self, method: &str, params: Value) -> Value {
            let id = self.next_id;
            self.next_id += 1;
            self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
                .await;
            loop {
                let message = self.line().await;
                if message["id"] == id {
                    return message;
                }
            }
        }

        /// The legacy handshake, answered with the shim's result.
        async fn initialize(&mut self) -> Value {
            let answered = self
                .request(
                    "initialize",
                    json!({
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": {"name": "test", "version": "0"},
                    }),
                )
                .await;
            self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .await;
            answered
        }

        async fn call(&mut self, tool: &str, arguments: Value) -> Value {
            self.request("tools/call", json!({"name": tool, "arguments": arguments}))
                .await
        }
    }

    /// What a client of the `2026-07-28` era puts on every request instead of
    /// a handshake.
    fn modern_meta() -> Value {
        json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {"name": "test-client", "version": "1.0.0"},
            "io.modelcontextprotocol/clientCapabilities": {},
        })
    }

    fn listed_names(response: &Value) -> Vec<&str> {
        let tools = response["result"]["tools"]
            .as_array()
            .expect("tools are listed");
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        }
        tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("a tool has a name"))
            .collect()
    }

    /// The text of a tool result that is an error.
    fn error_text(response: &Value) -> &str {
        assert_eq!(response["result"]["isError"], true, "{response}");
        response["result"]["content"][0]["text"]
            .as_str()
            .expect("a result carries text")
    }

    /// The app side: the real listener, with a consumer on a std thread that
    /// answers every call with `reply` and reports the tool it was asked for.
    struct App {
        path: std::path::PathBuf,
        listener: Option<Listener>,
        calls: mpsc::Receiver<Tool>,
    }

    impl App {
        fn answering(reply: Reply) -> Self {
            let path = short_socket_path();
            let (inbound, mut incoming) = tokio::sync::mpsc::unbounded_channel();
            let Ok(listener) = crate::agent::listen(&path, inbound) else {
                panic!("the socket binds");
            };
            let (seen, calls) = mpsc::channel();
            thread::spawn(move || {
                while let Some(message) = incoming.blocking_recv() {
                    if let crate::agent::Inbound::Call {
                        tool, reply: to, ..
                    } = message
                    {
                        let _ = seen.send(tool);
                        let _ = to.send(reply.clone());
                    }
                }
            });
            Self {
                path,
                listener: Some(listener),
                calls,
            }
        }
    }

    impl Drop for App {
        fn drop(&mut self) {
            self.listener.take();
            let _ = std::fs::remove_dir_all(self.path.parent().expect("the path has a directory"));
        }
    }

    #[test]
    fn the_shim_answers_the_initialize_handshake() {
        block_on(async {
            let mut client = Client::serve(Shim::new(Ok(short_socket_path())));
            let answered = client.initialize().await;
            assert_eq!(answered["result"]["serverInfo"]["name"], "dbdelve");
            assert_eq!(
                answered["result"]["serverInfo"]["version"],
                env!("CARGO_PKG_VERSION")
            );
            let listed = client.request("tools/list", json!({})).await;
            assert_eq!(listed_names(&listed), ["list_connections", "ping"]);
        });
    }

    #[test]
    fn the_shim_answers_server_discover() {
        block_on(async {
            let mut client = Client::serve(Shim::new(Ok(short_socket_path())));
            let discovered = client
                .request("server/discover", json!({"_meta": modern_meta()}))
                .await;
            let versions = discovered["result"]["supportedVersions"]
                .as_array()
                .expect("supported versions are listed");
            assert!(versions.contains(&json!("2026-07-28")), "{discovered}");
            assert!(versions.contains(&json!("2025-11-25")), "{discovered}");
            // In this era the server's name rides in the result's `_meta`.
            let info = &discovered["result"]["_meta"]["io.modelcontextprotocol/serverInfo"];
            assert_eq!(info["name"], "dbdelve", "{discovered}");
            assert_eq!(info["version"], env!("CARGO_PKG_VERSION"), "{discovered}");

            // No `initialize` was sent: each request carries its own version.
            let listed = client
                .request("tools/list", json!({"_meta": modern_meta()}))
                .await;
            assert_eq!(listed_names(&listed), ["list_connections", "ping"]);
        });
    }

    #[test]
    fn every_tool_answers_app_not_running_with_no_socket() {
        let missing = short_socket_path();

        // A socket file that nothing listens on, as an app that quit leaves.
        let stale = short_socket_path();
        std::fs::create_dir_all(stale.parent().expect("the path has a directory"))
            .expect("the directory is made");
        drop(UnixListener::bind(&stale).expect("the socket binds"));
        assert!(stale.exists());

        let too_long = "The agent socket path is 130 bytes long; this system allows at most 103.";
        let cases = [
            (Ok(missing), NOT_RUNNING),
            (Ok(stale.clone()), NOT_RUNNING),
            (Err(too_long.to_string()), too_long),
        ];
        for (socket, sentence) in cases {
            block_on(async {
                let mut client = Client::serve(Shim::new(socket));
                client.initialize().await;
                for (tool, arguments) in [
                    ("list_connections", json!({})),
                    ("ping", json!({"connection": "Orders"})),
                ] {
                    let answered = client.call(tool, arguments).await;
                    assert_eq!(
                        error_text(&answered),
                        format!("status: app_not_running\n{sentence}")
                    );
                }
            });
        }
        let _ = std::fs::remove_dir_all(stale.parent().expect("the path has a directory"));
    }

    #[test]
    fn a_call_rides_its_own_connection_and_returns_the_apps_answer() {
        let app = App::answering(Reply {
            status: Status::Executed,
            text: "[]".to_string(),
        });
        block_on(async {
            let mut client = Client::serve(Shim::new(Ok(app.path.clone())));
            client.initialize().await;
            let answered = client.call("list_connections", json!({})).await;
            assert_eq!(answered["result"]["isError"], false, "{answered}");
            assert_eq!(
                answered["result"]["content"][0]["text"],
                "status: executed\n[]"
            );
        });
        assert_eq!(app.calls.recv_timeout(PATIENCE), Ok(Tool::ListConnections));
        assert!(app.calls.try_recv().is_err());
    }
}

#[cfg(all(test, not(unix)))]
mod tests {
    use super::run;

    #[test]
    fn the_agent_command_exits_non_zero_on_windows() {
        assert_eq!(run(), 1);
    }
}
