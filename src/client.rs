use crate::cli::AuthMethod;
use reqwest::Client;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// Unwraps the Proxmox `{"data": ...}` envelope if present.
pub fn unwrap_response(mut val: Value) -> Value {
    if let Value::Object(ref mut map) = val {
        if let Some(data) = map.remove("data") {
            return data;
        }
    }
    val
}

/// Unwraps QEMU guest agent result envelope `{"result": ...}` and/or `{"data": ...}`.
pub fn unwrap_agent_result(mut val: Value) -> Value {
    val = unwrap_response(val);
    if let Value::Object(ref mut map) = val {
        if let Some(result) = map.remove("result") {
            return result;
        }
    }
    val
}

/// Extracts cluster name from `/cluster/status` or falls back to `"standalone"`.
pub fn parse_cluster_name(data: &Value) -> String {
    let list = if let Some(d) = data.get("data").and_then(|d| d.as_array()) {
        d
    } else if let Some(arr) = data.as_array() {
        arr
    } else {
        return "standalone".to_string();
    };

    for entry in list {
        if let Some(obj) = entry.as_object() {
            if obj.get("type").and_then(|v| v.as_str()) == Some("cluster") {
                return obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("standalone")
                    .to_string();
            }
        }
    }
    "standalone".to_string()
}

/// Parses HA resource list (`/cluster/ha/resources`) into numeric VMIDs set.
pub fn parse_ha_vmids(data: &Value) -> HashSet<u64> {
    let mut vmids = HashSet::new();
    let list = if let Some(d) = data.get("data").and_then(|d| d.as_array()) {
        d
    } else if let Some(arr) = data.as_array() {
        arr
    } else {
        return vmids;
    };

    for entry in list {
        if let Some(sid) = entry.get("sid").and_then(|s| s.as_str()) {
            if let Some(v_str) = sid.strip_prefix("vm:") {
                if let Ok(vmid) = v_str.parse::<u64>() {
                    vmids.insert(vmid);
                }
            }
        }
    }
    vmids
}

/// Parses guest OS details from `/agent/get-osinfo` output.
pub fn parse_guest_os(data: &Value) -> HashMap<String, Option<String>> {
    let mut map = HashMap::new();
    map.insert("os_family".to_string(), None);
    map.insert("os_distribution".to_string(), None);
    map.insert("os_version".to_string(), None);

    let obj = if let Some(r) = data.get("result").and_then(|r| r.as_object()) {
        r
    } else if let Some(o) = data.as_object() {
        o
    } else {
        return map;
    };

    if obj.is_empty() {
        return map;
    }

    let id = obj
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_lowercase())
        .filter(|s| !s.is_empty());

    let pretty_name = obj
        .get("pretty-name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let os_distribution = pretty_name.or(name).map(|s| s.to_string());

    let version_id = obj
        .get("version-id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let version = obj
        .get("version")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());
    let ver = version_id.or(version);

    let kernel = obj
        .get("kernel-release")
        .and_then(|k| k.as_str())
        .filter(|s| !s.is_empty());

    let os_version = match (ver, kernel) {
        (Some(v), Some(k)) => Some(format!("{} ({})", v, k)),
        (Some(v), None) => Some(v.to_string()),
        (None, Some(k)) => Some(k.to_string()),
        (None, None) => None,
    };

    map.insert("os_family".to_string(), id);
    map.insert("os_distribution".to_string(), os_distribution);
    map.insert("os_version".to_string(), os_version);

    map
}

/// Parses guest IP addresses from `/agent/network-get-interfaces` output.
pub fn parse_guest_ips(data: &Value) -> Vec<String> {
    let mut ips = Vec::new();
    let list = if let Some(r) = data.get("result").and_then(|r| r.as_array()) {
        r
    } else if let Some(arr) = data.as_array() {
        arr
    } else {
        return ips;
    };

    for iface in list {
        if let Some(addr_list) = iface.get("ip-addresses").and_then(|a| a.as_array()) {
            for entry in addr_list {
                if let Some(raw_ip) = entry.get("ip-address").and_then(|ip| ip.as_str()) {
                    let validated = crate::model::valid_ipv4(raw_ip);
                    if !validated.is_empty() && !ips.contains(&validated) {
                        ips.push(validated);
                    }
                }
            }
        }
    }
    ips
}

const VHOST_SCRIPT: &str = "for c in apachectl apache2ctl httpd; do command -v $c >/dev/null 2>&1 && { $c -S 2>&1; break; }; done; command -v nginx >/dev/null 2>&1 && nginx -T 2>/dev/null | sed 's/#.*//' | grep -oE 'server_name[[:space:]][^;]*;'; true";

/// Parses web vhost names from `apachectl -S` and `nginx -T | grep server_name` output.
/// Returns lowercase, deduplicated, sorted hostnames; drops `_`, localhost, IPs and dotless names.
pub fn parse_virtual_hosts(output: &str) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    let mut add = |tok: &str| {
        let t = tok.trim().trim_end_matches(';').to_lowercase();
        if t.is_empty()
            || t == "_"
            || t == "localhost"
            || t == "localhost.localdomain"
            || !t.contains('.')
            || t.parse::<std::net::IpAddr>().is_ok()
        {
            return;
        }
        set.insert(t);
    };
    for raw in output.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        let toks: Vec<&str> = line.split_whitespace().collect();
        match toks.as_slice() {
            ["server_name", names @ ..] => names.iter().for_each(|n| add(n)),
            [.., "namevhost", host, _] | [.., "namevhost", host] => add(host),
            ["alias", host] => add(host),
            [first, host, ..] if first.starts_with('*') && first.contains(':') => add(host),
            _ => {}
        }
    }
    set.into_iter().collect()
}

/// HTTP client for Proxmox VE REST API with ticket or API token authentication.
#[derive(Clone, Debug)]
pub struct ProxmoxClient {
    client: Client,
    base_url: String,
    auth: AuthMethod,
    ticket: Option<String>,
    csrf: Option<String>,
}

impl ProxmoxClient {
    /// Creates a new ProxmoxClient instance.
    ///
    /// Scheme normalization: If `host` lacks `http://` or `https://`, prepends `https://`.
    /// Trailing slashes are trimmed.
    pub fn new(
        host: &str,
        auth: AuthMethod,
        verify_ssl: bool,
        timeout_secs: u64,
    ) -> Result<Self, reqwest::Error> {
        let trimmed = host.trim().trim_end_matches('/');
        let base_url = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            trimmed.to_string()
        } else {
            format!("https://{}", trimmed)
        };

        let client = Client::builder()
            .danger_accept_invalid_certs(!verify_ssl)
            .timeout(Duration::from_secs(timeout_secs))
            .build()?;

        Ok(Self {
            client,
            base_url,
            auth,
            ticket: None,
            csrf: None,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn auth(&self) -> &AuthMethod {
        &self.auth
    }

    pub fn ticket(&self) -> Option<&str> {
        self.ticket.as_deref()
    }

    pub fn csrf(&self) -> Option<&str> {
        self.csrf.as_deref()
    }

    /// Authenticates with the Proxmox cluster using the configured `AuthMethod`.
    ///
    /// For `AuthMethod::ApiToken`, this is a no-op returning `Ok(())`.
    /// For `AuthMethod::Ticket`, sends `POST /api2/json/access/ticket` and saves ticket and CSRF token.
    pub async fn authenticate(&mut self, user: &str) -> Result<(), String> {
        match &self.auth {
            AuthMethod::ApiToken { .. } => Ok(()),
            AuthMethod::Ticket { password } => {
                let url = format!("{}/api2/json/access/ticket", self.base_url);
                let resp = self
                    .client
                    .post(&url)
                    .form(&[("username", user), ("password", password.as_str())])
                    .send()
                    .await
                    .map_err(|e| {
                        let msg = format!("Failed to authenticate to {}: {}", url, e);
                        eprintln!("[warn] {}", msg);
                        msg
                    })?;

                if !resp.status().is_success() {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    let msg = format!("Authentication failed with HTTP {}: {}", status, body);
                    eprintln!("[warn] {}", msg);
                    return Err(msg);
                }

                let body: Value = resp.json().await.map_err(|e| {
                    let msg = format!("Failed to parse ticket JSON: {}", e);
                    eprintln!("[warn] {}", msg);
                    msg
                })?;

                let data = body.get("data").ok_or_else(|| {
                    let msg = "Ticket response missing 'data'".to_string();
                    eprintln!("[warn] {}", msg);
                    msg
                })?;

                let ticket = data
                    .get("ticket")
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| {
                        let msg = "Ticket response missing 'ticket' string".to_string();
                        eprintln!("[warn] {}", msg);
                        msg
                    })?;

                let csrf = data
                    .get("CSRFPreventionToken")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");

                self.ticket = Some(ticket.to_string());
                self.csrf = Some(csrf.to_string());
                Ok(())
            }
        }
    }

    fn with_auth(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            AuthMethod::ApiToken { token } => {
                req = req.header("Authorization", format!("PVEAPIToken={}", token));
            }
            AuthMethod::Ticket { .. } => {
                if let Some(ticket) = &self.ticket {
                    req = req.header("Cookie", format!("PVEAuthCookie={}", ticket));
                }
                if let Some(csrf) = &self.csrf {
                    if !csrf.is_empty() {
                        req = req.header("CSRFPreventionToken", csrf);
                    }
                }
            }
        }
        req
    }

    async fn send_api(
        &self,
        method: &str,
        path: &str,
        req: reqwest::RequestBuilder,
    ) -> Result<Value, String> {
        let resp = self.with_auth(req).send().await.map_err(|e| {
            let msg = format!("{} {} failed: {}", method, path, e);
            eprintln!("[warn] {}", msg);
            msg
        })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let msg = format!("{} {} failed with HTTP {}: {}", method, path, status, text);
            eprintln!("[warn] {}", msg);
            return Err(msg);
        }

        let body: Value = resp.json().await.map_err(|e| {
            let msg = format!("{} {} failed to parse JSON: {}", method, path, e);
            eprintln!("[warn] {}", msg);
            msg
        })?;

        Ok(unwrap_response(body))
    }

    /// Performs an authenticated GET request against the given path and unwraps `data`.
    pub async fn api_get(&self, path: &str) -> Result<Value, String> {
        let req = self.client.get(format!("{}{}", self.base_url, path));
        self.send_api("GET", path, req).await
    }

    /// Performs an authenticated form POST against the given path and unwraps `data`.
    /// Repeated keys in `form` are sent repeatedly (PVE array params).
    pub async fn api_post(&self, path: &str, form: &[(&str, &str)]) -> Result<Value, String> {
        let req = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .form(form);
        self.send_api("POST", path, req).await
    }

    /// Performs an authenticated GET request for a QEMU guest agent endpoint, unwrapping `result`.
    pub async fn agent_get(&self, node: &str, vmid: u64, command: &str) -> Result<Value, String> {
        let path = format!("/api2/json/nodes/{}/qemu/{}/agent/{}", node, vmid, command);
        let val = self.api_get(&path).await?;
        Ok(unwrap_agent_result(val))
    }

    /// Fetches cluster name from `/cluster/status`, falling back to `"standalone"`.
    pub async fn get_cluster_name(&self) -> Result<String, String> {
        let data = self.api_get("/api2/json/cluster/status").await?;
        Ok(parse_cluster_name(&data))
    }

    /// Fetches online nodes from `/nodes`.
    pub async fn get_nodes(&self) -> Result<Vec<String>, String> {
        let data = self.api_get("/api2/json/nodes").await?;
        let list = match data {
            Value::Array(arr) => arr,
            Value::Null => Vec::new(),
            _ => return Err("nodes endpoint returned non-array".to_string()),
        };

        let nodes = list
            .iter()
            .filter(|n| n.get("status").and_then(|s| s.as_str()) == Some("online"))
            .filter_map(|n| n.get("node").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect();

        Ok(nodes)
    }

    /// Fetches all QEMU guest resources cluster-wide from `/cluster/resources?type=vm`.
    pub async fn get_cluster_vms(&self) -> Result<Vec<Value>, String> {
        let data = self.api_get("/api2/json/cluster/resources?type=vm").await?;
        let list = match data {
            Value::Array(arr) => arr,
            Value::Null => Vec::new(),
            _ => return Err("cluster/resources endpoint returned non-array".to_string()),
        };

        let filtered = list
            .into_iter()
            .filter(|r| {
                r.get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .starts_with("qemu/")
            })
            .collect();

        Ok(filtered)
    }

    /// Fallback: fetches VMs for a specific node from `/nodes/{node}/qemu`.
    pub async fn get_vms_for_node(&self, node: &str) -> Result<Vec<Value>, String> {
        let path = format!("/api2/json/nodes/{}/qemu", node);
        let data = self.api_get(&path).await?;
        match data {
            Value::Array(arr) => Ok(arr),
            Value::Null => Ok(Vec::new()),
            _ => Err(format!("{}/qemu endpoint returned non-array", node)),
        }
    }

    /// Fetches configuration dictionary for a VM from `/nodes/{node}/qemu/{vmid}/config`.
    pub async fn get_vm_config(&self, node: &str, vmid: u64) -> Result<Map<String, Value>, String> {
        let path = format!("/api2/json/nodes/{}/qemu/{}/config", node, vmid);
        let data = self.api_get(&path).await?;
        match data {
            Value::Object(map) => Ok(map),
            _ => Err(format!("config for {}/{} returned non-object", node, vmid)),
        }
    }

    /// Fetches storage list for a node from `/nodes/{node}/storage`.
    pub async fn get_storage_config(&self, node: &str) -> Result<Vec<Value>, String> {
        let path = format!("/api2/json/nodes/{}/storage", node);
        let data = self.api_get(&path).await?;
        match data {
            Value::Array(arr) => Ok(arr),
            Value::Null => Ok(Vec::new()),
            _ => Err(format!("{}/storage endpoint returned non-array", node)),
        }
    }

    /// Fetches image content for a specific storage from `/nodes/{node}/storage/{storage}/content?content=images`.
    pub async fn get_storage_content(&self, node: &str, storage: &str) -> Result<Vec<Value>, String> {
        let path = format!(
            "/api2/json/nodes/{}/storage/{}/content?content=images",
            node, storage
        );
        let data = self.api_get(&path).await?;
        match data {
            Value::Array(arr) => Ok(arr),
            Value::Null => Ok(Vec::new()),
            _ => Err(format!("{}/content endpoint returned non-array", storage)),
        }
    }

    /// Fetches backup jobs list from `/cluster/backup`.
    pub async fn get_backup_jobs(&self) -> Result<Vec<Value>, String> {
        let data = self.api_get("/api2/json/cluster/backup").await?;
        match data {
            Value::Array(arr) => Ok(arr),
            Value::Null => Ok(Vec::new()),
            _ => Err("cluster/backup endpoint returned non-array".to_string()),
        }
    }

    /// Fetches VMIDs managed by High Availability from `/cluster/ha/resources`.
    pub async fn get_ha_vmids(&self) -> HashSet<u64> {
        match self.api_get("/api2/json/cluster/ha/resources").await {
            Ok(data) => parse_ha_vmids(&data),
            Err(e) => {
                eprintln!("[warn] Failed to fetch HA resources: {}", e);
                HashSet::new()
            }
        }
    }

    /// Probes guest agent status via `/agent/info`. Returns agent info Value if live, else `None`.
    pub async fn get_agent_info(&self, node: &str, vmid: u64) -> Option<Value> {
        match self.agent_get(node, vmid, "info").await {
            Ok(val) => {
                if let Some(obj) = val.as_object() {
                    if let Some(ver) = obj.get("version") {
                        if !ver.is_null() && ver.as_str().map(|s| !s.is_empty()).unwrap_or(true) {
                            return Some(val);
                        }
                    }
                }
                None
            }
            Err(_) => None,
        }
    }

    /// Fetches guest IP addresses via `/agent/network-get-interfaces`.
    pub async fn get_guest_ips(&self, node: &str, vmid: u64) -> Vec<String> {
        match self.agent_get(node, vmid, "network-get-interfaces").await {
            Ok(data) => parse_guest_ips(&data),
            Err(_) => Vec::new(),
        }
    }

    /// Fetches guest OS information via `/agent/get-osinfo`.
    pub async fn get_guest_os(&self, node: &str, vmid: u64) -> HashMap<String, Option<String>> {
        match self.agent_get(node, vmid, "get-osinfo").await {
            Ok(data) => parse_guest_os(&data),
            Err(_) => {
                let mut map = HashMap::new();
                map.insert("os_family".to_string(), None);
                map.insert("os_distribution".to_string(), None);
                map.insert("os_version".to_string(), None);
                map
            }
        }
    }

    /// Lists web vhosts served by the guest via `/agent/exec` (apachectl -S, nginx -T). Soft-fails to empty.
    pub async fn get_guest_virtual_hosts(&self, node: &str, vmid: u64) -> Vec<String> {
        let base = format!("/api2/json/nodes/{}/qemu/{}/agent", node, vmid);
        let form = [
            ("command", "/bin/sh"),
            ("command", "-c"),
            ("command", VHOST_SCRIPT),
        ];
        let Ok(started) = self.api_post(&format!("{}/exec", base), &form).await else {
            return Vec::new();
        };
        let Some(pid) = started.get("pid").and_then(|p| p.as_u64()) else {
            return Vec::new();
        };
        let status_path = format!("{}/exec-status?pid={}", base, pid);
        for _ in 0..25 {
            let Ok(status) = self.api_get(&status_path).await else {
                return Vec::new();
            };
            let exited = match status.get("exited") {
                Some(Value::Bool(b)) => *b,
                Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
                _ => false,
            };
            if exited {
                let out = status.get("out-data").and_then(|v| v.as_str()).unwrap_or("");
                return parse_virtual_hosts(out);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Vec::new()
    }
}
