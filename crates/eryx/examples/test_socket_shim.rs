use eryx::Sandbox;

#[tokio::main]
async fn main() -> Result<(), eryx::Error> {
    let sandbox = Sandbox::embedded().build()?;

    let result = sandbox
        .execute(
            r#"
import socket
print("socket:", socket)
print("socket.__doc__:", socket.__doc__)
print("dir(socket):", [x for x in dir(socket) if not x.startswith('_')])
print("AddressFamily:", getattr(socket, 'AddressFamily', 'MISSING'))
print("FOO_TESTING_04_12:", getattr(socket, 'FOO_TESTING_04_12', 'MISSING'))

# Try re-executing the enum creation to see if it works at runtime
try:
    from enum import IntEnum
    class TestEnum(IntEnum):
        A = 1
    print(f"enum works: {TestEnum.A}")
except Exception as e:
    print(f"enum failed: {e}")

# Try exec'ing AddressFamily into socket manually
try:
    from enum import IntEnum
    exec("""
class AddressFamily(IntEnum):
    AF_UNSPEC = 0
    AF_INET = 2
""", socket.__dict__)
    print(f"manual exec worked: {socket.AddressFamily}")
except Exception as e:
    print(f"manual exec failed: {e}")

print("OK")
"#,
        )
        .await?;

    println!("stdout: {}", result.stdout);
    if !result.stderr.is_empty() {
        println!("stderr: {}", result.stderr);
    }

    assert!(
        result.stdout.contains("OK"),
        "AddressFamily import failed!\nstdout: {}\nstderr: {}",
        result.stdout,
        result.stderr
    );

    println!("PASS");
    Ok(())
}
