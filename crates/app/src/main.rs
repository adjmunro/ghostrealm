//! ghostrealm — terminal multiplexer. Thin entry point; logic lives in the lib.

fn main() -> anyhow::Result<()> {
    ghostrealm::run_cli()
}
