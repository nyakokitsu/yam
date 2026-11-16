use clap::{Parser, Subcommand};
use yam::publish::cli;

#[derive(Parser)]
#[command(
    name = "yam-publish",
    about = "Build and publish a YAM package from a PC"
)]
struct Cli {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Subcommand)]
enum CommandKind {
    /// Check publication rules without building an artifact.
    Check(cli::CheckArgs),
    /// Build a .yampkg for one platform.
    Pack(cli::PackArgs),
    /// Sign and upload an already-built .yampkg.
    Publish(cli::PublishArgs),
    /// Build every declared target and publish the artifacts.
    Release(cli::ReleaseArgs),
    /// Show the moderation state of a published artifact.
    Status(cli::StatusArgs),
    /// Store or inspect registry credentials.
    Auth(cli::AuthArgs),
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        CommandKind::Check(args) => cli::check(&args),
        CommandKind::Pack(args) => cli::pack(&args),
        CommandKind::Publish(args) => cli::publish(&args),
        CommandKind::Release(args) => cli::release(&args),
        CommandKind::Status(args) => cli::status(&args),
        CommandKind::Auth(args) => cli::auth(&args),
    }
}
