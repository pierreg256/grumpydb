//! # GrumpyShell — Interactive GrumpyDB REPL
//!
//! A JavaScript-like shell for exploring GrumpyDB interactively.
//! Documents are JSON objects, commands follow a familiar `db.collection.method()` syntax.
//!
//! ## Usage
//!
//! ```bash
//! cargo run --example grumpysh                           # launch REPL
//! cargo run --example grumpysh -- --data ./mydata        # custom data dir
//! cargo run --example grumpysh -- --eval "use test; db.users.count()"  # one-shot
//! ```

mod filter;
mod json_parser;
mod parser;
mod repl;

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let mut data_dir = PathBuf::from(".grumpysh_data");
    let mut eval_cmd: Option<String> = None;

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
            "--help" | "-h" => {
                println!("GrumpyShell — Interactive GrumpyDB REPL\n");
                println!("Usage: grumpysh [OPTIONS]\n");
                println!("Options:");
                println!("  --data <dir>    Data directory (default: .grumpysh_data)");
                println!("  --eval <cmds>   Execute commands and exit (semicolon-separated)");
                println!("  --help          Show this help");
                return;
            }
            _ => {}
        }
        i += 1;
    }

    let mut repl = repl::Repl::new(&data_dir);

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
    println!("GrumpyShell v{}", env!("CARGO_PKG_VERSION"));
    println!("Type 'help' for commands, 'exit' to quit.\n");

    let mut rl = match rustyline::DefaultEditor::new() {
        Ok(rl) => rl,
        Err(e) => {
            eprintln!("Failed to initialize line editor: {e}");
            return;
        }
    };

    // Load history
    let history_path = dirs_home().join(".grumpysh_history");
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
