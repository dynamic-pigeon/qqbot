mod catalog;
mod model;
mod router;
mod tree;

pub use catalog::{CatalogStore, CommandCatalog, CommandHelp, CommandMetadata};
pub use model::{
    AccessError, Command, CommandArguments, CommandContext, CommandError, CommandResult,
    MessageScope, MessageSource, Permission, check_access, render_command_error,
};
pub use router::{CommandRouter, extract_command_text};
pub use tree::{
    CommandRegistrationError, CommandTree, ResolveOutcome, ResolvedCommand, RouteError,
};
