#[rustfmt::skip]
pub mod game_media;
pub mod auth;
pub mod ops_board;
pub mod ops_metrics;
pub mod owner;
// Compile the isolated route handlers without mounting them in the default router.
pub mod game_lifecycle;
#[rustfmt::skip]
pub mod game_input;
pub mod activity_poolrad;
pub mod dash;
pub mod messages_read;
pub mod messages_write;
pub mod panes;
pub mod poolrad_dashboard;
pub mod search_attachments;
pub mod system;
#[rustfmt::skip]
pub mod game_state;

pub mod fleet;

mod response_error;
