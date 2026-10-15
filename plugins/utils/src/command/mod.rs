mod catalog;
mod model;
mod router;
mod tree;

pub use catalog::CommandCatalog;
pub(crate) use model::{
    AccessError, MessageSource, check_access, render_command_error, user_is_bot_admin,
};
pub use model::{Command, CommandContext, CommandError, CommandResult, MessageScope, Permission};
pub use router::CommandRouter;
pub use tree::{
    CommandRegistrationError, CommandTree, ResolveOutcome, ResolvedCommand, RouteError,
};

#[cfg(test)]
mod tests;
