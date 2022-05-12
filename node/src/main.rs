//! Substrate Node Template CLI library.
#![warn(missing_docs)]

mod chain_spec;
#[macro_use]
mod service;
mod cli;
mod command;
// mod command_helper;
mod rpc;
mod primitives;
mod constants;

fn main() -> sc_cli::Result<()> {
	command::run()
}
