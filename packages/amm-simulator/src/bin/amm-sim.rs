fn main() {
    if let Err(err) = soroban_amm_simulator::run() {
        eprintln!("{err}");
        let code = match err {
            soroban_amm_simulator::SimulationError::InvalidInput(_)
            | soroban_amm_simulator::SimulationError::InvalidToken { .. }
            | soroban_amm_simulator::SimulationError::InvalidFeeBps { .. }
            | soroban_amm_simulator::SimulationError::Json { .. }
            | soroban_amm_simulator::SimulationError::Csv { .. } => 2,
            _ => 3,
        };
        std::process::exit(code);
    }
}
