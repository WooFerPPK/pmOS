// xtask — PMos build orchestrator.
//
// Subcommands:
//   assemble-dist     Copy WASM + JS + HTML + headers into dist/
//   dev-server        Serve dist/ on http://localhost:8080 with COOP/COEP
//   gen-sab-layout    Generate web/src/shared/sab-layout.ts from abi constants
//   gen-font-assets   Generate deterministic terminal PBM atlases
//   gen-keymap-assets Generate deterministic PMKM v1 keymaps
//   package <crate>   Produce dist/pkgs/<crate>-<ver>.pmpkg.tar
//   push-sample       Push a built sample-app bundle into the running
//                     PMos's OPFS via the test harness
//
// This file implements the dispatch; each subcommand lives in its
// own module. Real work for assemble-dist and dev-server happens in
// T026 and T027 respectively.

use std::env;
use std::process::ExitCode;

mod assemble_dist;
mod cargo_target;
mod dev_server;
mod gen_font_assets;
mod gen_keymap_assets;
mod gen_sab_layout;
mod package;
mod push_sample;
#[cfg(test)]
mod release_assets;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let subcmd = match args.first().map(String::as_str) {
        Some(s) => s,
        None => {
            eprintln!("usage: xtask <subcommand> [args...]");
            eprintln!(
                "subcommands: assemble-dist dev-server gen-sab-layout gen-font-assets gen-keymap-assets package push-sample"
            );
            return ExitCode::FAILURE;
        }
    };
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();

    let result = match subcmd {
        "assemble-dist" => assemble_dist::run(&rest),
        "dev-server" => dev_server::run(&rest),
        "gen-sab-layout" => gen_sab_layout::run(&rest),
        "gen-font-assets" => gen_font_assets::run(&rest),
        "gen-keymap-assets" => gen_keymap_assets::run(&rest),
        "package" => package::run(&rest),
        "push-sample" => push_sample::run(&rest),
        other => {
            eprintln!("xtask: unknown subcommand '{other}'");
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}
