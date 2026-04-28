//! # grumpy-repl — Interactive GrumpyDB REPL
//!
//! A JavaScript-like shell for exploring GrumpyDB interactively.
//! Documents are JSON objects, commands follow a familiar `db.collection.method()` syntax.
//!
//! ## Usage
//!
//! ```bash
//! # Embedded mode (direct disk access, no server needed)
//! cargo run -p grumpy-repl                                        # launch REPL
//! cargo run -p grumpy-repl -- --data ./mydata                     # custom data dir
//! cargo run -p grumpy-repl -- --eval "use test; db.users.count()" # one-shot
//!
//! # Connected mode (TCP client to a running GrumpyDB server)
//! cargo run -p grumpy-repl -- --host localhost --port 6380 --tenant acme --user alice
//! ```

mod filter;
mod json_parser;
mod parser;
mod repl;
mod tcp_backend;

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let mut data_dir = PathBuf::from(".grumpy_repl_data");
    let mut eval_cmd: Option<String> = None;
    let mut host: Option<String> = None;
    let mut port: u16 = 6380;
    let mut tenant: Option<String> = None;
    let mut user: Option<String> = None;
    let mut password: Option<String> = None;
    let mut use_tls = false;
    let mut embedded = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--data" | "-d" => {
                i += 1;
                if i < args.len() {
                    data_dir = PathBuf::from(&args[i]);
                }
            }
            "--eval" | "-e" => {
                i += 1;
                if i < args.len() {
                    eval_cmd = Some(args[i].clone());
                }
            }
            "--host" => {
                i += 1;
                if i < args.len() {
                    host = Some(args[i].clone());
                }
            }
            "--port" => {
                i += 1;
                if i < args.len() {
                    port = args[i].parse().unwrap_or(6380);
                }
            }
            "--tenant" => {
                i += 1;
                if i < args.len() {
                    tenant = Some(args[i].clone());
                }
            }
            "--user" => {
                i += 1;
                if i < args.len() {
                    user = Some(args[i].clone());
                }
            }
            "--password" => {
                i += 1;
                if i < args.len() {
                    password = Some(args[i].clone());
                }
            }
            "--tls" => use_tls = true,
            "--no-tls" => use_tls = false,
            "--embedded" => embedded = true,
            "--help" | "-h" => {
                println!("grumpy-repl — Interactive GrumpyDB REPL\n");
                println!("Usage: grumpy-repl [OPTIONS]\n");
                println!("Embedded mode (default if no --host):");
                println!("  --data <dir>       Data directory (default: .grumpy_repl_data)");
                println!("  --embedded         Force embedded mode\n");
                println!("Connected mode (TCP to a GrumpyDB server):");
                println!("  --host <host>      Server hostname");
                println!("  --port <port>      Server port (default: 6380)");
                println!("  --tenant <name>    Tenant name");
                println!("  --user <name>      Username");
                println!("  --password <pass>  Password (or prompted interactively)");
                println!("  --tls / --no-tls   TLS toggle\n");
                println!("Common:");
                println!("  --eval <cmds>      Execute commands and exit (semicolon-separated)");
                println!("  --help             Show this help");
                return;
            }
            _ => {}
        }
        i += 1;
    }

    // Decide mode: connected if --host provided (and not --embedded)
    let connected_mode = host.is_some() && !embedded;

    let mut repl = if connected_mode {
        let host = host.unwrap();
        let tenant = tenant.unwrap_or_else(|| {
            eprint!("Tenant: ");
            read_line_stdin()
        });
        let user = user.unwrap_or_else(|| {
            eprint!("Username: ");
            read_line_stdin()
        });
        let password = password.unwrap_or_else(|| {
            eprint!("Password: ");
            read_line_stdin()
        });

        match tcp_backend::TcpBackend::connect(&host, port, use_tls, &tenant, &user, &password) {
            Ok(backend) => {
                println!("Connected to GrumpyDB at {host}:{port} (TLS: {use_tls})");
                println!("Authenticated as {user}@{tenant}\n");
                repl::Repl::with_tcp_backend(backend)
            }
            Err(e) => {
                eprintln!("Connection failed: {e}");
                return;
            }
        }
    } else {
        repl::Repl::new(&data_dir)
    };

    // --eval mode: execute commands and exit
    if let Some(cmds) = eval_cmd {
        for cmd in cmds.split(';') {
            let cmd = cmd.trim();
            if cmd.is_empty() {
                continue;
            }
            match repl.execute(cmd) {
                Some(output) if !output.is_empty() => println!("{output}"),
                Some(_) => {}
                None => return, // exit command
            }
        }
        return;
    }

    // Interactive mode with rustyline
    if connected_mode {
        println!(
            "grumpy-repl v{} (connected mode)",
            env!("CARGO_PKG_VERSION")
        );
    } else {
        println!("grumpy-repl v{} (embedded mode)", env!("CARGO_PKG_VERSION"));
    }
    println!("Type 'help' for commands, 'exit' to quit.\n");

    let mut rl = match rustyline::DefaultEditor::new() {
        Ok(rl) => rl,
        Err(e) => {
            eprintln!("Failed to initialize line editor: {e}");
            return;
        }
    };

    // Load history
    let history_path = dirs_home().join(".grumpy_repl_history");
    let _ = rl.load_history(&history_path);

    loop {
        let prompt = repl.prompt();
        match rl.readline(&prompt) {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(line);

                match repl.execute(line) {
                    Some(output) if !output.is_empty() => println!("{output}"),
                    Some(_) => {}
                    None => {
                        let _ = rl.save_history(&history_path);
                        break;
                    }
                }
            }
            Err(rustyline::error::ReadlineError::Interrupted) => {
                println!("Ctrl-C — use 'exit' to quit");
            }
            Err(rustyline::error::ReadlineError::Eof) => {
                let _ = rl.save_history(&history_path);
                break;
            }
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        }
    }
}

/// Returns the user's home directory.
fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Read a line from stdin (for prompting tenant/user/password).
fn read_line_stdin() -> String {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap_or(0);
    line.trim().to_string()
}
