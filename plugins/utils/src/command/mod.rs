mod access;
mod catalog;
mod model;
mod router;
mod tree;

pub use access::{AccessError, MessageSource, check_access, dispatch_if_allowed};
pub use catalog::{CatalogStore, CommandCatalog, CommandHelp, CommandMetadata};
pub use model::{
    Command, CommandArguments, CommandContext, CommandError, CommandResult, MessageScope,
    Permission, render_command_error,
};
pub use router::{CommandRouter, extract_command_text};
pub use tree::{
    CommandRegistrationError, CommandTree, ResolveOutcome, ResolvedCommand, RouteError,
};
