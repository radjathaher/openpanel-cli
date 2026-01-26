use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CommandTree {
    pub version: u32,
    pub base_url: String,
    pub resources: Vec<Resource>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Resource {
    pub name: String,
    pub ops: Vec<Operation>,
    #[serde(default)]
    pub groups: Vec<Group>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Group {
    pub name: String,
    pub ops: Vec<Operation>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Operation {
    pub name: String,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub body_defaults: Option<Value>,
    #[serde(default)]
    pub args: Vec<ArgDef>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ArgDef {
    pub name: String,
    pub flag: String,
    pub location: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub list: bool,
    pub value_type: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub header_name: Option<String>,
    #[serde(default)]
    pub nullable: bool,
}

pub fn load_command_tree() -> CommandTree {
    let raw = include_str!("../schemas/command_tree.json");
    serde_json::from_str(raw).expect("invalid command_tree.json")
}
