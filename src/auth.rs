use std::collections::BTreeSet;
use std::net::TcpStream;
use std::path::PathBuf;

use ssh2::Session;

use crate::config::AppConfig;
use crate::error::{err_with_source, Result};
use crate::error_info;

pub fn run_auth_check(config: &AppConfig) -> Result<String> {
    let endpoint = SshEndpoint::parse(&config.remote.host, config.remote.port);
    let mut report = AuthReport::new(config, &endpoint);
    let tcp = match TcpStream::connect(endpoint.address()) {
        Ok(tcp) => tcp,
        Err(source) => {
            report.line(format!("[auth] tcp=failed error={source}"));
            return Ok(report.finish());
        }
    };
    report.line("[auth] tcp=ok");

    let mut session = Session::new()
        .map_err(|source| err_with_source(error_info::REMOTE_SSH_CONNECT_FAILED, source))?;
    session.set_tcp_stream(tcp);
    if let Err(error) = session.handshake() {
        report.line(format!("[auth] handshake=failed error={error}"));
        return Ok(report.finish());
    }
    report.line("[auth] handshake=ok");

    match session.userauth_agent(&endpoint.user) {
        Ok(()) if session.authenticated() => {
            report.line("[auth] agent=ok authenticated=true");
            return Ok(report.finish());
        }
        Ok(()) => report.line("[auth] agent=ok authenticated=false"),
        Err(error) => report.line(format!("[auth] agent=failed error={error}")),
    }

    for key in identity_files(config) {
        let exists = key.exists();
        report.line(format!("[auth] key={} exists={exists}", key.display()));
        if !exists {
            continue;
        }
        let passphrase = passphrase(config);
        match session.userauth_pubkey_file(&endpoint.user, None, &key, passphrase.as_deref()) {
            Ok(()) if session.authenticated() => {
                report.line(format!(
                    "[auth] key_auth={} authenticated=true",
                    key.display()
                ));
                return Ok(report.finish());
            }
            Ok(()) => report.line(format!(
                "[auth] key_auth={} authenticated=false",
                key.display()
            )),
            Err(error) => report.line(format!("[auth] key_auth={} error={error}", key.display())),
        }
    }

    report.line("[auth] result=failed");
    Ok(report.finish())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SshEndpoint {
    user: String,
    host: String,
    port: u16,
}

impl SshEndpoint {
    fn parse(remote: &str, port: u16) -> Self {
        let (user, host) = match remote.split_once('@') {
            Some((user, host)) => (user.to_owned(), host.to_owned()),
            None => (default_user(), remote.to_owned()),
        };
        Self { user, host, port }
    }

    fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

struct AuthReport {
    lines: Vec<String>,
}

impl AuthReport {
    fn new(config: &AppConfig, endpoint: &SshEndpoint) -> Self {
        let mut report = Self { lines: Vec::new() };
        report.line(format!("[auth] remote={}", config.remote.host));
        report.line(format!("[auth] user={}", endpoint.user));
        report.line(format!("[auth] host={}", endpoint.host));
        report.line(format!("[auth] port={}", endpoint.port));
        report.line(format!(
            "[auth] configured_identity={}",
            empty_as_none(&config.remote.identity_file)
        ));
        report.line(format!(
            "[auth] passphrase_env={}",
            empty_as_none(&config.remote.passphrase_env)
        ));
        report
    }

    fn line(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    fn finish(self) -> String {
        self.lines.join("\n")
    }
}

fn identity_files(config: &AppConfig) -> Vec<PathBuf> {
    let mut keys = Vec::new();
    let mut seen = BTreeSet::new();
    if !config.remote.identity_file.is_empty() {
        push_key(
            &mut keys,
            &mut seen,
            PathBuf::from(&config.remote.identity_file),
        );
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        let ssh = PathBuf::from(home).join(".ssh");
        push_key(&mut keys, &mut seen, ssh.join("id_ed25519"));
        push_key(&mut keys, &mut seen, ssh.join("id_rsa"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let ssh = PathBuf::from(home).join(".ssh");
        push_key(&mut keys, &mut seen, ssh.join("id_ed25519"));
        push_key(&mut keys, &mut seen, ssh.join("id_rsa"));
    }
    keys
}

fn push_key(keys: &mut Vec<PathBuf>, seen: &mut BTreeSet<PathBuf>, key: PathBuf) {
    if seen.insert(key.clone()) {
        keys.push(key);
    }
}

fn passphrase(config: &AppConfig) -> Option<String> {
    if config.remote.passphrase_env.is_empty() {
        None
    } else {
        std::env::var(&config.remote.passphrase_env).ok()
    }
}

fn default_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "root".to_owned())
}

fn empty_as_none(value: &str) -> &str {
    if value.is_empty() {
        "<none>"
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::config::AppConfig;

    use super::{identity_files, SshEndpoint};

    #[test]
    fn parses_user_host_endpoint() {
        let endpoint = SshEndpoint::parse("root@10.0.0.2", 2222);

        assert_eq!(endpoint.user, "root");
        assert_eq!(endpoint.host, "10.0.0.2");
        assert_eq!(endpoint.address(), "10.0.0.2:2222");
    }

    #[test]
    fn configured_identity_file_is_first() {
        let mut config = AppConfig::template("root@example.com", 22, "/rdev/project");
        config.remote.identity_file = "C:\\Users\\me\\.ssh\\id_ed25519".to_owned();

        let keys = identity_files(&config);

        assert_eq!(
            keys.first(),
            Some(&PathBuf::from("C:\\Users\\me\\.ssh\\id_ed25519"))
        );
    }
}
