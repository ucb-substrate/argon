use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Error)]
pub enum Error {
    #[error("no route found")]
    NoRouteFound,
    #[error("location is blocked by another net")]
    Blocked,
    #[error("location is occupied by another net")]
    Occupied,
}
