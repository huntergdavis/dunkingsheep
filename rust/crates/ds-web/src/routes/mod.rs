#[rustfmt::skip]
pub mod game_media;
pub mod auth;
pub mod ops_metrics;
pub mod owner;
// Compile the isolated route handlers without mounting them in the default router.
pub mod game_lifecycle;
pub mod system;
