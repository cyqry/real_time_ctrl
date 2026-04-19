use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Ls{
    pub size: Option<u64>,
    pub filename: Option<String>,
    pub is_file: bool,
    pub created_date: Option<String>,
    pub modified_date: Option<String>,
}