mod accept_cli;
mod agent_presets;
mod cli;
mod collect_cli;
mod doctor;
mod exec;
mod mcp;
mod node;
mod profile_cli;
mod sell;
mod stub_pay_cli;
mod wallet_cli;

fn main() {
    let code = cli::run(
        std::env::args(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    std::process::exit(code);
}
