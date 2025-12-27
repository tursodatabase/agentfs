use agentfs_sdk::{AgentFS, AgentFSOptions, Stats};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::cmd::init::open_agentfs;

/// Main entry point for MCP server command
pub async fn handle_mcp_server_command(
    id_or_path: String,
    tools_filter: Option<Vec<String>>,
) -> Result<()> {
    // Resolve and open AgentFS
    let options = AgentFSOptions::resolve(&id_or_path).context(format!(
        "Failed to resolve agent ID or path: {}",
        id_or_path
    ))?;

    eprintln!("Using agent: {}", id_or_path);

    let (_, agentfs) = open_agentfs(options)
        .await
        .context("Failed to open AgentFS")?;

    // Create MCP server with tool filtering
    let server = McpServer::new(agentfs, tools_filter);

    // Run server with stdio transport
    eprintln!("Starting MCP server on stdio...");
    eprintln!("Protocol: Model Context Protocol (MCP) over JSON-RPC 2.0");
    server.serve().await?;

    Ok(())
}

/// MCP Server implementation
struct McpServer {
    agentfs: Arc<AgentFS>,
    enabled_tools: Option<HashSet<String>>,
}

impl McpServer {
    fn new(agentfs: AgentFS, tools_filter: Option<Vec<String>>) -> Self {
        let enabled_tools = tools_filter.map(|tools| {
            let set: HashSet<String> = tools.into_iter().collect();
            eprintln!("Tool filter enabled. Exposing tools: {:?}", set);
            set
        });

        if enabled_tools.is_none() {
            eprintln!("No tool filter specified. Exposing all tools.");
        }

        Self {
            agentfs: Arc::new(agentfs),
            enabled_tools,
        }
    }

    /// Check if a tool is enabled based on filter
    fn is_tool_enabled(&self, tool_name: &str) -> bool {
        match &self.enabled_tools {
            None => true, // No filter = all enabled
            Some(set) => set.contains(tool_name),
        }
    }

    async fn serve(self) -> Result<()> {
        let server = Arc::new(Mutex::new(self));
        let stdin = io::stdin();
        let mut stdout = io::stdout();

        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }

            // Parse JSON-RPC request
            let request: JsonValue = match serde_json::from_str(&line) {
                Ok(req) => req,
                Err(e) => {
                    eprintln!("Failed to parse JSON-RPC request: {}", e);
                    continue;
                }
            };

            // Handle request
            let response = server.lock().await.handle_request(request).await;

            // Write response to stdout
            if let Some(resp) = response {
                let resp_str = serde_json::to_string(&resp)?;
                writeln!(stdout, "{}", resp_str)?;
                stdout.flush()?;
            }
        }

        Ok(())
    }

    async fn handle_request(&self, request: JsonValue) -> Option<JsonValue> {
        // Extract request fields
        let method = request.get("method")?.as_str()?;
        let id = request.get("id").cloned();
        let params = request.get("params").cloned().unwrap_or(json!({}));

        eprintln!("Received request: method={}", method);

        // Handle method
        let result = match method {
            "initialize" => self.handle_initialize(params).await,
            "initialized" => {
                // Notification, no response needed
                return None;
            }
            "tools/list" => self.handle_tools_list().await,
            "tools/call" => self.handle_tools_call(params).await,
            "resources/list" => self.handle_resources_list().await,
            "resources/read" => self.handle_resources_read(params).await,
            _ => Err(anyhow::anyhow!("Unknown method: {}", method)),
        };

        // Build JSON-RPC response
        let response = match result {
            Ok(result) => {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result
                })
            }
            Err(e) => {
                eprintln!("Error handling {}: {}", method, e);
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32603,
                        "message": e.to_string()
                    }
                })
            }
        };

        Some(response)
    }

    async fn handle_initialize(&self, _params: JsonValue) -> Result<JsonValue> {
        Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {},
                "resources": {}
            },
            "serverInfo": {
                "name": "agentfs-mcp-server",
                "version": env!("CARGO_PKG_VERSION")
            }
        }))
    }

    async fn handle_tools_list(&self) -> Result<JsonValue> {
        let tools = self.get_tool_definitions();
        Ok(json!({ "tools": tools }))
    }

    async fn handle_tools_call(&self, params: JsonValue) -> Result<JsonValue> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing tool name"))?;

        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        let result_text = match name {
            "read_file" => {
                let params: ReadFileParams = serde_json::from_value(arguments)?;
                self.handle_read_file(params).await?
            }
            "write_file" => {
                let params: WriteFileParams = serde_json::from_value(arguments)?;
                self.handle_write_file(params).await?
            }
            "readdir" => {
                let params: ReaddirParams = serde_json::from_value(arguments)?;
                self.handle_readdir(params).await?
            }
            "mkdir" => {
                let params: MkdirParams = serde_json::from_value(arguments)?;
                self.handle_mkdir(params).await?
            }
            "rename" => {
                let params: RenameParams = serde_json::from_value(arguments)?;
                self.handle_rename(params).await?
            }
            "remove" => {
                let params: RemoveParams = serde_json::from_value(arguments)?;
                self.handle_remove(params).await?
            }
            "stat" => {
                let params: StatParams = serde_json::from_value(arguments)?;
                self.handle_stat(params).await?
            }
            "access" => {
                let params: AccessParams = serde_json::from_value(arguments)?;
                self.handle_access(params).await?
            }
            "kv_get" => {
                let params: KvGetParams = serde_json::from_value(arguments)?;
                self.handle_kv_get(params).await?
            }
            "kv_set" => {
                let params: KvSetParams = serde_json::from_value(arguments)?;
                self.handle_kv_set(params).await?
            }
            "kv_delete" => {
                let params: KvDeleteParams = serde_json::from_value(arguments)?;
                self.handle_kv_delete(params).await?
            }
            _ => anyhow::bail!("Unknown tool: {}", name),
        };

        Ok(json!({
            "content": [
                {
                    "type": "text",
                    "text": result_text
                }
            ]
        }))
    }

    async fn handle_resources_list(&self) -> Result<JsonValue> {
        let resources = self.list_resources().await?;
        Ok(json!({ "resources": resources }))
    }

    async fn handle_resources_read(&self, params: JsonValue) -> Result<JsonValue> {
        let uri = params
            .get("uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing resource uri"))?;

        let contents = self.read_resource(uri).await?;
        let mime_type = guess_mime_type(uri);

        // Try to decode as UTF-8, fall back to base64
        let resource_contents = if let Ok(text) = String::from_utf8(contents.clone()) {
            json!({
                "uri": uri,
                "mimeType": mime_type,
                "text": text
            })
        } else {
            json!({
                "uri": uri,
                "mimeType": mime_type,
                "blob": base64_encode(&contents)
            })
        };

        Ok(json!({ "contents": [resource_contents] }))
    }

    fn get_tool_definitions(&self) -> Vec<JsonValue> {
        let mut tools = Vec::new();

        // Filesystem tools
        if self.is_tool_enabled("read_file") {
            tools.push(json!({
                "name": "read_file",
                "description": "Read file contents from the filesystem",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to read"
                        },
                        "encoding": {
                            "type": "string",
                            "enum": ["utf8", "base64"],
                            "description": "Encoding to use for file contents (default: utf8)"
                        }
                    },
                    "required": ["path"]
                }
            }));
        }

        if self.is_tool_enabled("write_file") {
            tools.push(json!({
                "name": "write_file",
                "description": "Write content to a file in the filesystem",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to write"
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to write to the file"
                        },
                        "encoding": {
                            "type": "string",
                            "enum": ["utf8", "base64"],
                            "description": "Encoding of the content (default: utf8)"
                        },
                        "create_dirs": {
                            "type": "boolean",
                            "description": "Create parent directories if they don't exist"
                        }
                    },
                    "required": ["path", "content"]
                }
            }));
        }

        if self.is_tool_enabled("readdir") {
            tools.push(json!({
                "name": "readdir",
                "description": "List contents of a directory",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the directory to list"
                        }
                    },
                    "required": ["path"]
                }
            }));
        }

        if self.is_tool_enabled("mkdir") {
            tools.push(json!({
                "name": "mkdir",
                "description": "Create a directory",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path of the directory to create"
                        }
                    },
                    "required": ["path"]
                }
            }));
        }

        if self.is_tool_enabled("remove") {
            tools.push(json!({
                "name": "remove",
                "description": "Remove a file or empty directory",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path of the file or directory to remove"
                        }
                    },
                    "required": ["path"]
                }
            }));
        }

        if self.is_tool_enabled("rename") {
            tools.push(json!({
                "name": "rename",
                "description": "Move or rename a file or directory",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "from": {
                            "type": "string",
                            "description": "Source path"
                        },
                        "to": {
                            "type": "string",
                            "description": "Destination path"
                        }
                    },
                    "required": ["from", "to"]
                }
            }));
        }

        if self.is_tool_enabled("stat") {
            tools.push(json!({
                "name": "stat",
                "description": "Get file or directory metadata",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to stat"
                        }
                    },
                    "required": ["path"]
                }
            }));
        }

        if self.is_tool_enabled("access") {
            tools.push(json!({
                "name": "access",
                "description": "Test if a path exists",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to test"
                        }
                    },
                    "required": ["path"]
                }
            }));
        }

        // KV store tools
        if self.is_tool_enabled("kv_get") {
            tools.push(json!({
                "name": "kv_get",
                "description": "Get a value from the key-value store",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "Key to retrieve"
                        }
                    },
                    "required": ["key"]
                }
            }));
        }

        if self.is_tool_enabled("kv_set") {
            tools.push(json!({
                "name": "kv_set",
                "description": "Set a value in the key-value store",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "Key to set"
                        },
                        "value": {
                            "description": "Value to store (any JSON value)"
                        }
                    },
                    "required": ["key", "value"]
                }
            }));
        }

        if self.is_tool_enabled("kv_delete") {
            tools.push(json!({
                "name": "kv_delete",
                "description": "Delete a key from the key-value store",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "Key to delete"
                        }
                    },
                    "required": ["key"]
                }
            }));
        }

        if self.is_tool_enabled("kv_list") {
            tools.push(json!({
                "name": "kv_list",
                "description": "List keys in the key-value store with optional prefix",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prefix": {
                            "type": "string",
                            "description": "Optional prefix to filter keys"
                        }
                    }
                }
            }));
        }

        tools
    }
}

// ============================================================================
// Helper functions
// ============================================================================

/// Normalize a path to ensure it starts with /
fn normalize_path(path: &str) -> Result<String> {
    let path = path.trim();

    // Reject paths with .. for security
    if path.contains("..") {
        anyhow::bail!("Path traversal not allowed: {}", path);
    }

    // Convert relative to absolute
    let normalized = if path.starts_with('/') {
        path.to_string()
    } else if path.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", path)
    };

    Ok(normalized)
}

/// Guess MIME type based on file extension
fn guess_mime_type(path: &str) -> String {
    match Path::new(path).extension().and_then(|s| s.to_str()) {
        Some("txt") | Some("md") => "text/plain",
        Some("json") => "application/json",
        Some("html") | Some("htm") => "text/html",
        Some("js") | Some("mjs") => "application/javascript",
        Some("ts") => "application/typescript",
        Some("css") => "text/css",
        Some("xml") => "application/xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("tar") => "application/x-tar",
        Some("gz") => "application/gzip",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// Base64 encode bytes
fn base64_encode(data: &[u8]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    STANDARD.encode(data)
}

/// Base64 decode string
fn base64_decode(s: &str) -> Result<Vec<u8>> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    STANDARD
        .decode(s)
        .map_err(|e| anyhow::anyhow!("Base64 decode error: {}", e))
}

// ============================================================================
// Tool parameter types
// ============================================================================

#[derive(Debug, Serialize, Deserialize)]
struct ReadFileParams {
    path: String,
    #[serde(default)]
    encoding: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WriteFileParams {
    path: String,
    content: String,
    #[serde(default)]
    encoding: Option<String>,
    #[serde(default)]
    create_dirs: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReaddirParams {
    path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct MkdirParams {
    path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RenameParams {
    from: String,
    to: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RemoveParams {
    path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StatParams {
    path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct AccessParams {
    path: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct KvGetParams {
    key: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct KvSetParams {
    key: String,
    value: JsonValue,
}

#[derive(Debug, Serialize, Deserialize)]
struct KvDeleteParams {
    key: String,
}

// ============================================================================
// Tool implementations
// ============================================================================

impl McpServer {
    /// Read file contents
    async fn handle_read_file(&self, params: ReadFileParams) -> Result<String> {
        let path = normalize_path(&params.path)?;

        let data = self
            .agentfs
            .fs
            .read_file(&path)
            .await
            .context("Failed to read file")?
            .ok_or_else(|| anyhow::anyhow!("File not found: {}", path))?;

        let content = match params.encoding.as_deref() {
            Some("base64") => base64_encode(&data),
            _ => String::from_utf8(data)
                .context("File is not valid UTF-8. Use encoding=base64 for binary files.")?,
        };

        Ok(content)
    }

    /// Write file contents
    async fn handle_write_file(&self, params: WriteFileParams) -> Result<String> {
        let path = normalize_path(&params.path)?;

        let data = match params.encoding.as_deref() {
            Some("base64") => base64_decode(&params.content)?,
            _ => params.content.into_bytes(),
        };

        // Create parent directories if requested
        if params.create_dirs.unwrap_or(false) {
            Box::pin(self.ensure_parent_dirs(&path)).await?;
        }

        self.agentfs
            .fs
            .write_file(&path, &data)
            .await
            .context("Failed to write file")?;

        Ok(format!("Wrote {} bytes to {}", data.len(), path))
    }

    /// Helper to create parent directories recursively
    fn ensure_parent_dirs<'a>(
        &'a self,
        path: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let path_obj = Path::new(path);
            let parent = match path_obj.parent() {
                Some(p) if !p.as_os_str().is_empty() && p != Path::new("/") => p,
                _ => return Ok(()),
            };

            let parent_str = parent.to_string_lossy().to_string();
            let parent_path = normalize_path(&parent_str)?;

            // Check if parent exists
            if self.agentfs.fs.stat(&parent_path).await?.is_some() {
                return Ok(());
            }

            // Recursively ensure grandparent exists
            self.ensure_parent_dirs(&parent_path).await?;

            // Create parent
            self.agentfs
                .fs
                .mkdir(&parent_path)
                .await
                .context(format!("Failed to create directory: {}", parent_path))?;

            Ok(())
        })
    }

    /// List directory contents
    async fn handle_readdir(&self, params: ReaddirParams) -> Result<String> {
        let path = normalize_path(&params.path)?;

        let entries = self
            .agentfs
            .fs
            .readdir(&path)
            .await
            .context("Failed to read directory")?
            .ok_or_else(|| anyhow::anyhow!("Directory not found: {}", path))?;

        Ok(serde_json::to_string_pretty(&entries)?)
    }

    /// Create directory
    async fn handle_mkdir(&self, params: MkdirParams) -> Result<String> {
        let path = normalize_path(&params.path)?;

        self.agentfs
            .fs
            .mkdir(&path)
            .await
            .context("Failed to create directory")?;

        Ok(format!("Created directory: {}", path))
    }

    /// Remove empty directory
    async fn handle_remove(&self, params: RemoveParams) -> Result<String> {
        let path = normalize_path(&params.path)?;

        self.agentfs
            .fs
            .remove(&path)
            .await
            .context("Failed to remove directory")?;

        Ok(format!("Removed directory: {}", path))
    }

    /// Rename/move file or directory
    async fn handle_rename(&self, params: RenameParams) -> Result<String> {
        let from = normalize_path(&params.from)?;
        let to = normalize_path(&params.to)?;

        self.agentfs
            .fs
            .rename(&from, &to)
            .await
            .context("Failed to rename")?;

        Ok(format!("Renamed {} to {}", from, to))
    }

    /// Get file metadata
    async fn handle_stat(&self, params: StatParams) -> Result<String> {
        let path = normalize_path(&params.path)?;

        let stats = self
            .agentfs
            .fs
            .stat(&path)
            .await
            .context("Failed to stat")?
            .ok_or_else(|| anyhow::anyhow!("Path not found: {}", path))?;

        Ok(serde_json::to_string_pretty(&StatsResponse::from(stats))?)
    }

    /// Test if path exists
    async fn handle_access(&self, params: AccessParams) -> Result<String> {
        let path = normalize_path(&params.path)?;

        let exists = self.agentfs.fs.stat(&path).await?.is_some();

        Ok(serde_json::to_string(&json!({ "exists": exists }))?)
    }

    /// Get KV value
    async fn handle_kv_get(&self, params: KvGetParams) -> Result<String> {
        let value: Option<JsonValue> = self
            .agentfs
            .kv
            .get(&params.key)
            .await
            .context("Failed to get value")?;

        Ok(serde_json::to_string_pretty(&value)?)
    }

    /// Set KV value
    async fn handle_kv_set(&self, params: KvSetParams) -> Result<String> {
        self.agentfs
            .kv
            .set(&params.key, &params.value)
            .await
            .context("Failed to set value")?;

        Ok(format!("Set key: {}", params.key))
    }

    /// Delete KV value
    async fn handle_kv_delete(&self, params: KvDeleteParams) -> Result<String> {
        self.agentfs
            .kv
            .delete(&params.key)
            .await
            .context("Failed to delete key")?;

        Ok(format!("Deleted key: {}", params.key))
    }

    /// List all files as resources
    async fn list_resources(&self) -> Result<Vec<JsonValue>> {
        let mut resources = Vec::new();
        Box::pin(self.collect_file_resources("/", &mut resources)).await?;
        Ok(resources)
    }

    /// Recursively collect file resources
    fn collect_file_resources<'a>(
        &'a self,
        path: &'a str,
        resources: &'a mut Vec<JsonValue>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let entries = match self.agentfs.fs.readdir(path).await? {
                Some(entries) => entries,
                None => return Ok(()),
            };

            for entry in entries {
                let full_path = if path == "/" {
                    format!("/{}", entry)
                } else {
                    format!("{}/{}", path, entry)
                };

                let stats = match self.agentfs.fs.stat(&full_path).await? {
                    Some(s) => s,
                    None => continue,
                };

                if stats.is_file() {
                    resources.push(json!({
                        "uri": full_path,
                        "name": entry,
                        "description": format!("File at {}", full_path),
                        "mimeType": guess_mime_type(&full_path)
                    }));
                } else if stats.is_directory() {
                    // Recurse into subdirectory
                    self.collect_file_resources(&full_path, resources).await?;
                }
            }

            Ok(())
        })
    }

    /// Read a resource by path
    async fn read_resource(&self, path: &str) -> Result<Vec<u8>> {
        let normalized = normalize_path(path)?;

        let data = self
            .agentfs
            .fs
            .read_file(&normalized)
            .await
            .context("Failed to read file")?
            .ok_or_else(|| anyhow::anyhow!("File not found: {}", normalized))?;

        Ok(data)
    }
}

// ============================================================================
// Response types
// ============================================================================

#[derive(Debug, Serialize)]
struct StatsResponse {
    ino: i64,
    mode: u32,
    nlink: u32,
    uid: u32,
    gid: u32,
    size: i64,
    atime: i64,
    mtime: i64,
    ctime: i64,
    is_file: bool,
    is_directory: bool,
    is_symlink: bool,
}

impl From<Stats> for StatsResponse {
    fn from(stats: Stats) -> Self {
        Self {
            ino: stats.ino,
            mode: stats.mode,
            nlink: stats.nlink,
            uid: stats.uid,
            gid: stats.gid,
            size: stats.size,
            atime: stats.atime,
            mtime: stats.mtime,
            ctime: stats.ctime,
            is_file: stats.is_file(),
            is_directory: stats.is_directory(),
            is_symlink: stats.is_symlink(),
        }
    }
}
