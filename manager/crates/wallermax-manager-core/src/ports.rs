//! Who already listens on the address the server wants to bind — the
//! pre-flight that turns "os error 10048" into a name and a way out.
//!
//! [`probe`] connect-probes the bind addresses: anything that accepts a
//! connection is a squatter. [`listening_owners`] then asks the OS who
//! the squatter is — on Windows, through a captured, windowless
//! `netstat -ano` (the LISTENING rows on the port) plus one `tasklist`
//! pass for the image names; on other systems the OS is not asked and
//! the answer is "an unknown process" (the diagnostics still name the
//! port and the way out). Both parsers are pure functions over plain
//! text, so their edge cases are tested on every platform, not just on
//! the one that produces the text.

use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use serde::Serialize;

/// One process the OS names as listening on a port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PortOwner {
    pub pid: u32,
    /// The image name, e.g. `wallermax-server.exe`.
    pub image: String,
}

/// What a probe of the bind addresses found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occupancy {
    /// Every address refused the connection — nothing is listening.
    Free,
    /// This address accepted a connection — someone is already there.
    Busy(SocketAddr),
}

/// Connect-probes `addresses`; the first that accepts a connection is
/// the squatter. A refused connection means no listener, so an empty
/// slice is vacuously free.
///
/// A short timeout matters: this runs between a Start click and the
/// spawn, so the pre-flight must never feel like a hang.
pub fn probe(addresses: &[SocketAddr], timeout: Duration) -> Occupancy {
    for address in addresses {
        if TcpStream::connect_timeout(address, timeout).is_ok() {
            return Occupancy::Busy(*address);
        }
    }
    Occupancy::Free
}

/// The processes listening on `port`, when the OS can name them.
///
/// Windows answers through two captured, windowless system tools; on
/// any other OS the answer is "unknown" — an empty list, never a guess.
pub fn listening_owners(port: u16) -> Vec<PortOwner> {
    #[cfg(windows)]
    {
        windows_listening_owners(port)
    }
    #[cfg(not(windows))]
    {
        let _ = port;
        Vec::new()
    }
}

/// The Windows answer: the LISTENING pids from `netstat -ano -p tcp`,
/// each named by one `tasklist` pass.
#[cfg(windows)]
fn windows_listening_owners(port: u16) -> Vec<PortOwner> {
    use std::collections::BTreeMap;

    let Ok(output) = captured("netstat").args(["-ano", "-p", "tcp"]).output() else {
        return Vec::new();
    };
    let table = String::from_utf8_lossy(&output.stdout);
    let mut pids: Vec<u32> = table
        .lines()
        .filter_map(parse_netstat_listening)
        .filter(|(local, _)| *local == port)
        .map(|(_, pid)| pid)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    if pids.is_empty() {
        return Vec::new();
    }

    let Ok(output) = captured("tasklist").args(["/FO", "CSV", "/NH"]).output() else {
        return Vec::new();
    };
    let table = String::from_utf8_lossy(&output.stdout);
    let images: BTreeMap<u32, String> = table.lines().filter_map(parse_tasklist_row).collect();

    pids.into_iter()
        .map(|pid| PortOwner {
            pid,
            image: images
                .get(&pid)
                .cloned()
                .unwrap_or_else(|| String::from("<unknown>")),
        })
        .collect()
}

/// A captured, windowless console command (see `process_manager` for
/// the flag's rationale: the manager is a GUI and must never flash a
/// console).
#[cfg(windows)]
fn captured(program: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = std::process::Command::new(program);
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

/// Parses one `netstat -ano` row into `(local port, pid)` when it is a
/// LISTENING TCP row:
/// `TCP  127.0.0.1:8080  0.0.0.0:0  LISTENING  4123`.
///
/// The state names are protocol constants — never localized — while the
/// header row is, so matching the five-field LISTENING shape is immune
/// to any locale. Only called on Windows, but exercised by the tests
/// everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_netstat_listening(line: &str) -> Option<(u16, u32)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() != 5 || fields[0] != "TCP" || fields[3] != "LISTENING" {
        return None;
    }
    let port = fields[1].rsplit(':').next()?.parse().ok()?;
    let pid = fields[4].parse().ok()?;
    Some((port, pid))
}

/// Parses one `tasklist /FO CSV /NH` row into `(pid, image)`:
/// `"wallermax-server.exe","4123","Console","1","5,120 K"`.
///
/// Quote-aware, so a comma inside an image name stays inside the name.
/// Only called on Windows, but exercised by the tests everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_tasklist_row(line: &str) -> Option<(u32, String)> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for character in line.chars() {
        match character {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => fields.push(std::mem::take(&mut current)),
            _ => current.push(character),
        }
    }
    fields.push(current);
    let pid = fields.get(1)?.trim().parse().ok()?;
    Some((pid, fields[0].clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_listener_is_busy_a_dropped_one_is_free() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        assert_eq!(
            probe(&[address], Duration::from_millis(300)),
            Occupancy::Busy(address),
            "the live listener answers"
        );
        drop(listener);
        // A dropped *listener* leaves no TIME_WAIT (only connected
        // sockets do); a tiny retry only guards against another process
        // grabbing the port mid-test.
        let free = (0..10).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            probe(&[address], Duration::from_millis(300)) == Occupancy::Free
        });
        assert!(free, "the port is free again after the drop");
    }

    #[test]
    fn probing_nothing_is_vacuously_free() {
        assert_eq!(probe(&[], Duration::from_millis(10)), Occupancy::Free);
    }

    #[test]
    fn netstat_listening_rows_parse() {
        assert_eq!(
            parse_netstat_listening("  TCP    127.0.0.1:8080    0.0.0.0:0    LISTENING    4123"),
            Some((8080, 4123))
        );
        assert_eq!(
            parse_netstat_listening("  TCP    [::1]:8080        [::]:0       LISTENING    4123"),
            Some((8080, 4123))
        );
        assert_eq!(
            parse_netstat_listening("  TCP    0.0.0.0:8080      1.2.3.4:5    ESTABLISHED  99"),
            None,
            "only LISTENING rows count"
        );
        assert_eq!(
            parse_netstat_listening("  UDP    127.0.0.1:8080    *:*    4123"),
            None,
            "UDP rows and headers never match the five-field TCP shape"
        );
        assert_eq!(parse_netstat_listening(""), None);
    }

    #[test]
    fn tasklist_rows_parse() {
        assert_eq!(
            parse_tasklist_row("\"wallermax-server.exe\",\"4123\",\"Console\",\"1\",\"5,120 K\""),
            Some((4123, String::from("wallermax-server.exe")))
        );
        assert_eq!(
            parse_tasklist_row("\"weird,name.exe\",\"77\",\"Console\",\"1\",\"1 K\""),
            Some((77, String::from("weird,name.exe"))),
            "a comma inside the quoted name stays in the name"
        );
        assert_eq!(
            parse_tasklist_row("INFO: No tasks are running which match the specified criteria."),
            None,
            "tasklist's failure line is not a row"
        );
        assert_eq!(parse_tasklist_row(""), None);
    }
}
