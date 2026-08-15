use std::{
    env, fs,
    io::{Read, Write},
    process, thread,
    time::{Duration, Instant},
};

fn main() {
    if let Err(error) = run() {
        eprintln!("fake agent failed: {error}");
        process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let mut exit_code = 0;
    let mut sleep_ms = 0;
    let mut before_stderr_ms = 0;
    let mut wait_before_stderr_file = None;
    let mut wait_before_stdout_bytes_file = None;
    let mut stdout_text = None;
    let mut stderr_text = None;
    let mut stdout_bytes = 0;
    let mut invalid_stderr = false;
    let mut split_utf8 = false;
    let mut stdin_to_stdout = false;
    let mut touch_marker = None;
    let mut child_marker = None;
    let mut spawn_child_marker = None;

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--exit" => exit_code = required(&mut args, "--exit")?.parse()?,
            "--sleep-ms" => sleep_ms = required(&mut args, "--sleep-ms")?.parse()?,
            "--before-stderr-ms" => {
                before_stderr_ms = required(&mut args, "--before-stderr-ms")?.parse()?;
            }
            "--wait-before-stderr-file" => {
                wait_before_stderr_file = Some(required(&mut args, "--wait-before-stderr-file")?);
            }
            "--wait-before-stdout-bytes-file" => {
                wait_before_stdout_bytes_file =
                    Some(required(&mut args, "--wait-before-stdout-bytes-file")?);
            }
            "--stdout" => stdout_text = Some(required(&mut args, "--stdout")?),
            "--stderr" => stderr_text = Some(required(&mut args, "--stderr")?),
            "--stdout-bytes" => stdout_bytes = required(&mut args, "--stdout-bytes")?.parse()?,
            "--invalid-stderr" => invalid_stderr = true,
            "--split-utf8" => split_utf8 = true,
            "--stdin-to-stdout" => stdin_to_stdout = true,
            "--touch-marker" => touch_marker = Some(required(&mut args, "--touch-marker")?),
            "--child-marker" => child_marker = Some(required(&mut args, "--child-marker")?),
            "--spawn-child-marker" => {
                spawn_child_marker = Some(required(&mut args, "--spawn-child-marker")?);
            }
            _ => return Err(format!("unknown argument {argument:?}").into()),
        }
    }

    if let Some(marker) = child_marker {
        fs::write(marker, process::id().to_string())?;
        thread::sleep(Duration::from_secs(30));
        return Ok(());
    }

    if let Some(marker) = touch_marker {
        fs::write(marker, process::id().to_string())?;
    }

    let mut spawned_child = if let Some(marker) = spawn_child_marker {
        Some(
            process::Command::new(env::current_exe()?)
                .args(["--child-marker", &marker])
                .spawn()?,
        )
    } else {
        None
    };

    if stdin_to_stdout {
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input)?;
        std::io::stdout().write_all(&input)?;
        std::io::stdout().flush()?;
    }

    if let Some(text) = stdout_text {
        std::io::stdout().write_all(text.as_bytes())?;
        std::io::stdout().flush()?;
    }
    if split_utf8 {
        let emoji = "🦀".as_bytes();
        std::io::stdout().write_all(&emoji[..2])?;
        std::io::stdout().flush()?;
        thread::sleep(Duration::from_millis(20));
        std::io::stdout().write_all(&emoji[2..])?;
        std::io::stdout().flush()?;
    }
    if before_stderr_ms > 0 {
        thread::sleep(Duration::from_millis(before_stderr_ms));
    }
    if let Some(path) = wait_before_stderr_file {
        wait_for_path(&path)?;
    }
    if let Some(text) = stderr_text {
        std::io::stderr().write_all(text.as_bytes())?;
        std::io::stderr().flush()?;
    }
    if invalid_stderr {
        std::io::stderr().write_all(&[0xff, b'\n'])?;
        std::io::stderr().flush()?;
    }
    if let Some(path) = wait_before_stdout_bytes_file {
        wait_for_path(&path)?;
    }
    if stdout_bytes > 0 {
        let chunk = vec![b'x'; 64 * 1024];
        let mut remaining = stdout_bytes;
        while remaining > 0 {
            let count = remaining.min(chunk.len());
            std::io::stdout().write_all(&chunk[..count])?;
            remaining -= count;
        }
        std::io::stdout().flush()?;
    }
    if sleep_ms > 0 {
        thread::sleep(Duration::from_millis(sleep_ms));
    }

    if let Some(child) = spawned_child.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    process::exit(exit_code);
}

fn wait_for_path(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::path::Path::new(path).exists() {
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {path}").into());
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

fn required(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    arguments
        .next()
        .ok_or_else(|| format!("{option} requires a value").into())
}
