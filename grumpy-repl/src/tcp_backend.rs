//! TCP backend: translates grumpy-repl commands to the GrumpyDB wire protocol
//! via the `grumpydb-client` driver.

use grumpydb_client::{ClientError, GrumpyClient};
use grumpydb_protocol::Response;

/// A TCP backend that communicates with a GrumpyDB server.
pub struct TcpBackend {
    rt: tokio::runtime::Runtime,
    client: GrumpyClient,
}

impl TcpBackend {
    /// Connect to a GrumpyDB server and authenticate.
    pub fn connect(
        host: &str,
        port: u16,
        tls: bool,
        tenant: &str,
        username: &str,
        password: &str,
    ) -> Result<Self, String> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        let client = rt
            .block_on(async {
                let mut c = GrumpyClient::connect(host, port, tls).await?;
                c.login(tenant, username, password).await?;
                Ok::<_, ClientError>(c)
            })
            .map_err(|e| e.to_string())?;

        Ok(Self { rt, client })
    }

    /// Select a database (sends USE command).
    pub fn use_database(&mut self, name: &str) -> Result<String, String> {
        self.rt
            .block_on(async {
                let _db = self.client.database(name).await?;
                Ok::<_, ClientError>(format!("Switched to database \"{name}\""))
            })
            .map_err(|e| e.to_string())
    }

    /// Execute a raw protocol command and return the formatted response.
    pub fn execute_raw(&mut self, cmd: &str) -> Result<String, String> {
        self.rt
            .block_on(async {
                let resp = self.client.raw_execute(cmd).await?;
                Ok(format_response(&resp))
            })
            .map_err(|e: ClientError| e.to_string())
    }

    /// Close the connection.
    pub fn close(&mut self) {
        let _ = self.rt.block_on(self.client.close());
    }
}

/// Format a protocol Response for display in the shell.
fn format_response(resp: &Response) -> String {
    match resp {
        Response::Ok(msg) => {
            if msg == "OK" {
                "OK".to_string()
            } else {
                msg.clone()
            }
        }
        Response::Error(msg) => format!("Error: {msg}"),
        Response::Integer(n) => n.to_string(),
        Response::Bulk(None) => "null".to_string(),
        Response::Bulk(Some(data)) => {
            // Try to pretty-print if it looks like JSON
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(data) {
                serde_json::to_string_pretty(&val).unwrap_or_else(|_| data.clone())
            } else {
                data.clone()
            }
        }
        Response::Array(items) => {
            if items.is_empty() {
                return "[]".to_string();
            }
            // Check if it's a list of bulk strings (key-value pairs or plain strings)
            let formatted: Vec<String> = items.iter().map(format_response).collect();
            // Try to parse each as JSON for pretty display
            let json_items: Vec<serde_json::Value> = formatted
                .iter()
                .map(|s| {
                    // Try "uuid json" format from SCAN/QUERY results
                    if let Some((key, json_str)) = s.split_once(' ') {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
                            let mut obj = serde_json::Map::new();
                            obj.insert(
                                "_id".to_string(),
                                serde_json::Value::String(key.to_string()),
                            );
                            if let serde_json::Value::Object(fields) = val {
                                for (k, v) in fields {
                                    obj.insert(k, v);
                                }
                            }
                            return serde_json::Value::Object(obj);
                        }
                    }
                    serde_json::Value::String(s.clone())
                })
                .collect();
            serde_json::to_string_pretty(&json_items).unwrap_or_else(|_| format!("{formatted:?}"))
        }
    }
}
