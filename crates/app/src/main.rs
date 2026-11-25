// The musl builds (the Linux release binaries and the container) use mimalloc: musl's allocator
// kept freed memory resident until the service reached its memory limit.
#[cfg(target_env = "musl")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    std::process::exit(energie::cli::main(std::env::args().skip(1).collect()));
}
