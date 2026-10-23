//! `hermes kanban …` — lightweight Kanban board management over a JSON store.
//!
//! Ported from NousResearch/hermes-agent's `hermes_cli/kanban.py` (core subset:
//! boards, lanes and cards). The original persists to SQLite via `kanban_db.py`;
//! this port keeps a plain JSON document at `$HERMES_HOME/kanban.json` so no new
//! (native/SQLite) dependency is needed.
//!
//! Usage:
//!
//! ```text
//! hermes kanban board create "Sprint 3" --description "March sprint"
//! hermes kanban board list
//! hermes kanban board show "Sprint 3"
//! hermes kanban board delete "Sprint 3" --yes
//!
//! hermes kanban lane list --board "Sprint 3"
//! hermes kanban lane create "Review" --board "Sprint 3"
//!
//! hermes kanban card create --title "Write docs" --board "Sprint 3"
//! hermes kanban card list --board "Sprint 3"
//! hermes kanban card show --card <id> --board "Sprint 3"
//! hermes kanban card move --card <id> --to "In Progress" --board "Sprint 3"
//! hermes kanban card done --card <id> --board "Sprint 3"
//! hermes kanban card delete --card <id> --board "Sprint 3" --yes
//! ```
//!
//! Boards are addressed by name (or id); cards by id, unique id-prefix, or exact
//! title. New boards start with `Todo`, `In Progress` and `Done` lanes. Moving a
//! card into a lane named "done" marks it done; moving it back out re-opens it.

use chrono::Utc;
use hermes_config::hermes_home;
use hermes_core::AgentError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Name of the JSON store file, relative to the hermes home.
pub const STORE_NAME: &str = "kanban.json";

/// Store schema version written into the JSON document.
const STORE_VERSION: u32 = 1;

/// Lanes seeded onto every new board (a small subset of the original's
/// `kanban_workflow.DEFAULT_WORKFLOW` columns).
const DEFAULT_LANES: [&str; 3] = ["Todo", "In Progress", "Done"];

/// Lane name treated as the "completed" bucket (case-insensitive).
const DONE_LANE_NAME: &str = "done";

// ---------------------------------------------------------------------------
// CLI surface
// ---------------------------------------------------------------------------

/// `hermes kanban …` arguments. Wired by the parent into the `CliCommand` enum.
#[derive(clap::Args, Debug, Clone)]
pub struct KanbanArgs {
    /// Kanban action group.
    #[command(subcommand)]
    pub command: KanbanCommand,
}

/// Top-level action groups.
#[derive(Clone, clap::Subcommand, Debug)]
pub enum KanbanCommand {
    /// Manage boards.
    Board {
        #[command(subcommand)]
        action: BoardAction,
    },
    /// Manage lanes on a board.
    Lane {
        #[command(subcommand)]
        action: LaneAction,
    },
    /// Manage cards on a board.
    Card {
        #[command(subcommand)]
        action: CardAction,
    },
}

/// `hermes kanban board …` actions.
#[derive(Clone, clap::Subcommand, Debug)]
pub enum BoardAction {
    /// Create a board (seeded with Todo / In Progress / Done lanes).
    Create {
        /// Board name (unique).
        name: String,
        /// Optional board description.
        #[arg(long)]
        description: Option<String>,
    },
    /// List all boards.
    List,
    /// Show a board's lanes and cards.
    Show {
        /// Board name or id.
        name: String,
    },
    /// Delete a board and all its cards. Requires `--yes`.
    Delete {
        /// Board name or id.
        name: String,
        /// Confirm deletion.
        #[arg(long)]
        yes: bool,
    },
}

/// `hermes kanban lane …` actions.
#[derive(Clone, clap::Subcommand, Debug)]
pub enum LaneAction {
    /// List the lanes of a board.
    List {
        /// Board name or id.
        #[arg(long, default_value = "default")]
        board: String,
    },
    /// Create a lane on a board.
    Create {
        /// Lane name (unique per board).
        name: String,
        /// Board name or id.
        #[arg(long, default_value = "default")]
        board: String,
    },
}

/// `hermes kanban card …` actions.
#[derive(Clone, clap::Subcommand, Debug)]
pub enum CardAction {
    /// Create a card in a lane.
    Create {
        /// Card title.
        #[arg(long)]
        title: String,
        /// Board name or id.
        #[arg(long, default_value = "default")]
        board: String,
        /// Optional card description.
        #[arg(long)]
        description: Option<String>,
        /// Lane name or id (defaults to the first lane).
        #[arg(long)]
        lane: Option<String>,
    },
    /// List cards, grouped by lane (hides done cards unless `--all`).
    List {
        /// Board name or id.
        #[arg(long, default_value = "default")]
        board: String,
        /// Only cards in this lane (name or id).
        #[arg(long)]
        lane: Option<String>,
        /// Include done cards.
        #[arg(long)]
        all: bool,
    },
    /// Show a card's details.
    Show {
        /// Card id, unique id-prefix, or exact title.
        #[arg(long)]
        card: String,
        /// Board name or id.
        #[arg(long, default_value = "default")]
        board: String,
    },
    /// Move a card to another lane (moving into the done lane completes it).
    Move {
        /// Card id, unique id-prefix, or exact title.
        #[arg(long)]
        card: String,
        /// Destination lane name or id.
        #[arg(long)]
        to: String,
        /// Board name or id.
        #[arg(long, default_value = "default")]
        board: String,
    },
    /// Mark a card done (moves it into the done lane).
    Done {
        /// Card id, unique id-prefix, or exact title.
        #[arg(long)]
        card: String,
        /// Board name or id.
        #[arg(long, default_value = "default")]
        board: String,
    },
    /// Delete a card. Requires `--yes`.
    Delete {
        /// Card id, unique id-prefix, or exact title.
        #[arg(long)]
        card: String,
        /// Board name or id.
        #[arg(long, default_value = "default")]
        board: String,
        /// Confirm deletion.
        #[arg(long)]
        yes: bool,
    },
}

// ---------------------------------------------------------------------------
// Store model
// ---------------------------------------------------------------------------

/// Top-level JSON document.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KanbanData {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub boards: Vec<Board>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Board {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub lanes: Vec<Lane>,
    #[serde(default)]
    pub cards: Vec<Card>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lane {
    pub id: String,
    pub name: String,
    pub position: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Card {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub lane_id: String,
    #[serde(default)]
    pub done: bool,
    pub position: usize,
    pub created_at: String,
    pub updated_at: String,
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

fn new_id(prefix: &str) -> String {
    format!("{}-{}", prefix, &Uuid::new_v4().to_string()[..8])
}

fn kb_err(msg: impl Into<String>) -> AgentError {
    AgentError::Io(msg.into())
}

/// JSON-backed kanban store. Loaded once per command, mutated in memory, then
/// saved atomically (temp file + rename) at the end of the run.
pub struct KanbanStore {
    path: PathBuf,
    data: KanbanData,
}

impl KanbanStore {
    /// Open the default store at `$HERMES_HOME/kanban.json`.
    pub fn load_default() -> Result<Self, AgentError> {
        Self::at(hermes_home().join(STORE_NAME))
    }

    /// Open a store at an explicit path (used by tests).
    pub fn at(path: impl Into<PathBuf>) -> Result<Self, AgentError> {
        let path = path.into();
        let data = Self::read_file(&path)?;
        Ok(Self { path, data })
    }

    fn read_file(path: &Path) -> Result<KanbanData, AgentError> {
        if !path.exists() {
            return Ok(KanbanData {
                version: STORE_VERSION,
                boards: Vec::new(),
            });
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| kb_err(format!("Failed to read {}: {}", path.display(), e)))?;
        serde_json::from_str(&raw)
            .map_err(|e| kb_err(format!("Failed to parse {}: {}", path.display(), e)))
    }

    /// Persist the in-memory state atomically (write temp file, then rename).
    pub fn save(&self) -> Result<(), AgentError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| kb_err(format!("Failed to create hermes home: {}", e)))?;
        }
        let raw = serde_json::to_string_pretty(&self.data)
            .map_err(|e| kb_err(format!("Failed to serialize kanban store: {}", e)))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, raw)
            .map_err(|e| kb_err(format!("Failed to write {}: {}", tmp.display(), e)))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| kb_err(format!("Failed to save {}: {}", self.path.display(), e)))?;
        Ok(())
    }

    /// Number of boards (test helper).
    #[cfg(test)]
    fn board_count(&self) -> usize {
        self.data.boards.len()
    }

    fn find_board_mut(&mut self, key: &str) -> Option<&mut Board> {
        self.data
            .boards
            .iter_mut()
            .find(|b| b.name == key || b.id == key)
    }

    fn find_board(&self, key: &str) -> Option<&Board> {
        self.data
            .boards
            .iter()
            .find(|b| b.name == key || b.id == key)
    }

    // -- boards -----------------------------------------------------------

    /// Create a board seeded with the default lanes. Errors when a board with
    /// the same name already exists. Returns the new board id.
    pub fn create_board(
        &mut self,
        name: &str,
        description: Option<&str>,
    ) -> Result<String, AgentError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(kb_err("Board name must not be empty"));
        }
        if self.data.boards.iter().any(|b| b.name == name) {
            return Err(kb_err(format!("Board '{}' already exists", name)));
        }
        let now = now_rfc3339();
        let board = Board {
            id: new_id("board"),
            name: name.to_string(),
            description: description.map(str::to_string),
            lanes: DEFAULT_LANES
                .iter()
                .enumerate()
                .map(|(i, lane)| Lane {
                    id: new_id("lane"),
                    name: (*lane).to_string(),
                    position: i,
                })
                .collect(),
            cards: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
        };
        let id = board.id.clone();
        self.data.boards.push(board);
        Ok(id)
    }

    /// Delete a board by name or id. Returns the deleted board's name.
    pub fn delete_board(&mut self, key: &str) -> Result<String, AgentError> {
        let pos = self
            .data
            .boards
            .iter()
            .position(|b| b.name == key || b.id == key)
            .ok_or_else(|| kb_err(format!("Board '{}' not found", key)))?;
        Ok(self.data.boards.remove(pos).name)
    }

    // -- lanes ------------------------------------------------------------

    /// Create a lane on a board. Returns the new lane id.
    pub fn create_lane(&mut self, board_key: &str, name: &str) -> Result<String, AgentError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(kb_err("Lane name must not be empty"));
        }
        let board = self
            .find_board_mut(board_key)
            .ok_or_else(|| kb_err(format!("Board '{}' not found", board_key)))?;
        if board
            .lanes
            .iter()
            .any(|l| l.name.eq_ignore_ascii_case(name))
        {
            return Err(kb_err(format!(
                "Lane '{}' already exists on board '{}'",
                name, board.name
            )));
        }
        let position = board.lanes.len();
        let lane = Lane {
            id: new_id("lane"),
            name: name.to_string(),
            position,
        };
        let id = lane.id.clone();
        board.updated_at = now_rfc3339();
        board.lanes.push(lane);
        Ok(id)
    }

    fn resolve_lane<'a>(board: &'a Board, key: &str) -> Result<&'a Lane, AgentError> {
        board
            .lanes
            .iter()
            .find(|l| l.id == key || l.name.eq_ignore_ascii_case(key))
            .ok_or_else(|| {
                kb_err(format!(
                    "Lane '{}' not found on board '{}'",
                    key, board.name
                ))
            })
    }

    fn resolve_lane_mut<'a>(board: &'a mut Board, key: &str) -> Result<&'a mut Lane, AgentError> {
        let name = board.name.clone();
        board
            .lanes
            .iter_mut()
            .find(|l| l.id == key || l.name.eq_ignore_ascii_case(key))
            .ok_or_else(|| kb_err(format!("Lane '{}' not found on board '{}'", key, name)))
    }

    /// Lane named "done" (case-insensitive), creating it when missing.
    fn done_lane_mut(board: &mut Board) -> &mut Lane {
        let pos = board.lanes.len();
        if !board
            .lanes
            .iter()
            .any(|l| l.name.eq_ignore_ascii_case(DONE_LANE_NAME))
        {
            board.lanes.push(Lane {
                id: new_id("lane"),
                name: "Done".to_string(),
                position: pos,
            });
        }
        board
            .lanes
            .iter_mut()
            .find(|l| l.name.eq_ignore_ascii_case(DONE_LANE_NAME))
            .expect("done lane was just ensured")
    }

    // -- cards ------------------------------------------------------------

    /// Create a card on a board (in `lane`, or the first lane). Returns the new
    /// card id.
    #[allow(clippy::too_many_arguments)]
    pub fn create_card(
        &mut self,
        board_key: &str,
        title: &str,
        description: Option<&str>,
        lane: Option<&str>,
    ) -> Result<String, AgentError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(kb_err("Card title must not be empty"));
        }
        let board = self
            .find_board_mut(board_key)
            .ok_or_else(|| kb_err(format!("Board '{}' not found", board_key)))?;
        let lane = match lane {
            Some(key) => Self::resolve_lane(board, key)?.id.clone(),
            None => board
                .lanes
                .first()
                .ok_or_else(|| kb_err(format!("Board '{}' has no lanes", board.name)))?
                .id
                .clone(),
        };
        let position = board.cards.iter().filter(|c| c.lane_id == lane).count();
        let now = now_rfc3339();
        let card = Card {
            id: new_id("card"),
            title: title.to_string(),
            description: description.map(str::to_string),
            lane_id: lane,
            done: false,
            position,
            created_at: now.clone(),
            updated_at: now,
        };
        let id = card.id.clone();
        board.updated_at = now_rfc3339();
        board.cards.push(card);
        Ok(id)
    }

    /// Resolve a card by id, unique id-prefix, or exact title (case-insensitive).
    fn resolve_card<'a>(board: &'a Board, key: &str) -> Result<&'a Card, AgentError> {
        let key = key.trim();
        let matches: Vec<&Card> = board
            .cards
            .iter()
            .filter(|c| c.id == key || c.id.starts_with(key) || c.title.eq_ignore_ascii_case(key))
            .collect();
        match matches.as_slice() {
            [one] => Ok(one),
            [] => Err(kb_err(format!(
                "Card '{}' not found on board '{}'",
                key, board.name
            ))),
            _ => {
                let ids: Vec<&str> = matches.iter().map(|c| c.id.as_str()).collect();
                Err(kb_err(format!(
                    "Card '{}' is ambiguous on board '{}': {}",
                    key,
                    board.name,
                    ids.join(", ")
                )))
            }
        }
    }

    /// Move a card to another lane. Moving into the done lane completes the
    /// card; moving out of it re-opens it.
    pub fn move_card(
        &mut self,
        board_key: &str,
        card_key: &str,
        lane_key: &str,
    ) -> Result<(), AgentError> {
        let card_id = {
            let board = self
                .find_board(board_key)
                .ok_or_else(|| kb_err(format!("Board '{}' not found", board_key)))?;
            Self::resolve_card(board, card_key)?.id.clone()
        };
        let target = {
            let board = self
                .find_board_mut(board_key)
                .ok_or_else(|| kb_err(format!("Board '{}' not found", board_key)))?;
            Self::resolve_lane_mut(board, lane_key)?.id.clone()
        };
        let board = self
            .find_board_mut(board_key)
            .ok_or_else(|| kb_err(format!("Board '{}' not found", board_key)))?;
        let target_is_done = board
            .lanes
            .iter()
            .find(|l| l.id == target)
            .is_some_and(|l| l.name.eq_ignore_ascii_case(DONE_LANE_NAME));
        let position = board.cards.iter().filter(|c| c.lane_id == target).count();
        let card = board
            .cards
            .iter_mut()
            .find(|c| c.id == card_id)
            .expect("card id was just resolved");
        card.lane_id = target;
        card.position = position;
        card.done = target_is_done;
        card.updated_at = now_rfc3339();
        board.updated_at = now_rfc3339();
        Ok(())
    }

    /// Mark a card done (moves it into the done lane, creating it if missing).
    pub fn complete_card(&mut self, board_key: &str, card_key: &str) -> Result<(), AgentError> {
        let card_id = {
            let board = self
                .find_board(board_key)
                .ok_or_else(|| kb_err(format!("Board '{}' not found", board_key)))?;
            Self::resolve_card(board, card_key)?.id.clone()
        };
        let target = {
            let board = self
                .find_board_mut(board_key)
                .ok_or_else(|| kb_err(format!("Board '{}' not found", board_key)))?;
            Self::done_lane_mut(board).id.clone()
        };
        let board = self
            .find_board_mut(board_key)
            .ok_or_else(|| kb_err(format!("Board '{}' not found", board_key)))?;
        let position = board.cards.iter().filter(|c| c.lane_id == target).count();
        let card = board
            .cards
            .iter_mut()
            .find(|c| c.id == card_id)
            .expect("card id was just resolved");
        card.lane_id = target;
        card.position = position;
        card.done = true;
        card.updated_at = now_rfc3339();
        board.updated_at = now_rfc3339();
        Ok(())
    }

    /// Delete a card by id, unique id-prefix, or exact title. Returns the
    /// deleted card's title.
    pub fn delete_card(&mut self, board_key: &str, card_key: &str) -> Result<String, AgentError> {
        let board = self
            .find_board_mut(board_key)
            .ok_or_else(|| kb_err(format!("Board '{}' not found", board_key)))?;
        let card_id = Self::resolve_card(board, card_key)?.id.clone();
        let pos = board
            .cards
            .iter()
            .position(|c| c.id == card_id)
            .expect("card id was just resolved");
        let title = board.cards.remove(pos).title;
        board.updated_at = now_rfc3339();
        Ok(title)
    }
}

// ---------------------------------------------------------------------------
// Command dispatch
// ---------------------------------------------------------------------------

/// Entry point for `hermes kanban …`.
pub async fn run(args: &KanbanArgs) -> anyhow::Result<()> {
    let mut store = KanbanStore::load_default().map_err(anyhow::Error::from)?;
    let dirty = match &args.command {
        KanbanCommand::Board { action } => handle_board(&mut store, action)?,
        KanbanCommand::Lane { action } => handle_lane(&mut store, action)?,
        KanbanCommand::Card { action } => handle_card(&mut store, action)?,
    };
    if dirty {
        store.save().map_err(anyhow::Error::from)?;
    }
    Ok(())
}

fn handle_board(store: &mut KanbanStore, action: &BoardAction) -> Result<bool, AgentError> {
    match action {
        BoardAction::Create { name, description } => {
            let id = store.create_board(name, description.as_deref())?;
            println!("📋 Board '{}' created (id: {})", name.trim(), id);
            Ok(true)
        }
        BoardAction::List => {
            if store.data.boards.is_empty() {
                println!("No boards yet. Create one with `hermes kanban board create <name>`.");
            } else {
                println!("Boards:");
                for board in &store.data.boards {
                    let open = board.cards.iter().filter(|c| !c.done).count();
                    let done = board.cards.iter().filter(|c| c.done).count();
                    println!(
                        "  • {} ({} lanes, {} open, {} done)",
                        board.name,
                        board.lanes.len(),
                        open,
                        done
                    );
                }
            }
            Ok(false)
        }
        BoardAction::Show { name } => {
            let board = store
                .find_board(name)
                .ok_or_else(|| kb_err(format!("Board '{}' not found", name)))?;
            print_board(board);
            Ok(false)
        }
        BoardAction::Delete { name, yes } => {
            if !yes {
                println!("Refusing to delete board '{}' without --yes.", name);
                return Ok(false);
            }
            let name = store.delete_board(name)?;
            println!("🗑️  Board '{}' deleted.", name);
            Ok(true)
        }
    }
}

fn handle_lane(store: &mut KanbanStore, action: &LaneAction) -> Result<bool, AgentError> {
    match action {
        LaneAction::List { board } => {
            let board = store
                .find_board(board)
                .ok_or_else(|| kb_err(format!("Board '{}' not found", board)))?;
            let mut lanes = board.lanes.clone();
            lanes.sort_by_key(|l| l.position);
            println!("Lanes on board '{}':", board.name);
            for lane in lanes {
                let count = board.cards.iter().filter(|c| c.lane_id == lane.id).count();
                println!("  • {} ({} cards)", lane.name, count);
            }
            Ok(false)
        }
        LaneAction::Create { name, board } => {
            let id = store.create_lane(board, name)?;
            let board = store.find_board(board).expect("board was just resolved");
            println!(
                "➕ Lane '{}' created on board '{}' (id: {})",
                name.trim(),
                board.name,
                id
            );
            Ok(true)
        }
    }
}

fn handle_card(store: &mut KanbanStore, action: &CardAction) -> Result<bool, AgentError> {
    match action {
        CardAction::Create {
            title,
            board,
            description,
            lane,
        } => {
            let id = store.create_card(board, title, description.as_deref(), lane.as_deref())?;
            let board = store.find_board(board).expect("board was just resolved");
            println!(
                "📝 Card '{}' created on board '{}' (id: {})",
                title.trim(),
                board.name,
                id
            );
            Ok(true)
        }
        CardAction::List { board, lane, all } => {
            let board = store
                .find_board(board)
                .ok_or_else(|| kb_err(format!("Board '{}' not found", board)))?;
            let lane_filter = match lane {
                Some(key) => Some(KanbanStore::resolve_lane(board, key)?.id.clone()),
                None => None,
            };
            print_board_filtered(board, lane_filter.as_deref(), *all);
            Ok(false)
        }
        CardAction::Show { card, board } => {
            let board = store
                .find_board(board)
                .ok_or_else(|| kb_err(format!("Board '{}' not found", board)))?;
            let card = KanbanStore::resolve_card(board, card)?;
            print_card(board, card);
            Ok(false)
        }
        CardAction::Move { card, to, board } => {
            store.move_card(board, card, to)?;
            let board = store.find_board(board).expect("board was just resolved");
            println!("➡️  Card moved to lane '{}' on board '{}'.", to, board.name);
            Ok(true)
        }
        CardAction::Done { card, board } => {
            store.complete_card(board, card)?;
            let board = store.find_board(board).expect("board was just resolved");
            println!("✅ Card marked done on board '{}'.", board.name);
            Ok(true)
        }
        CardAction::Delete { card, board, yes } => {
            if !yes {
                println!("Refusing to delete card without --yes.");
                return Ok(false);
            }
            let title = store.delete_card(board, card)?;
            println!("🗑️  Card '{}' deleted.", title);
            Ok(true)
        }
    }
}

fn print_board(board: &Board) {
    println!("📋 {}", board.name);
    if let Some(desc) = &board.description {
        println!("   {}", desc);
    }
    print_board_filtered(board, None, true);
}

fn print_board_filtered(board: &Board, lane_filter: Option<&str>, show_done: bool) {
    let mut lanes: Vec<&Lane> = board.lanes.iter().collect();
    lanes.sort_by_key(|l| l.position);
    for lane in lanes {
        if let Some(filter) = lane_filter {
            if lane.id != filter {
                continue;
            }
        }
        let mut cards: Vec<&Card> = board
            .cards
            .iter()
            .filter(|c| c.lane_id == lane.id && (show_done || !c.done))
            .collect();
        cards.sort_by_key(|c| c.position);
        println!("  {} ({})", lane.name, cards.len());
        for card in cards {
            let mark = if card.done { "✓" } else { "○" };
            println!("    {} {} [{}]", mark, card.title, card.id);
        }
    }
}

fn print_card(board: &Board, card: &Card) {
    let lane = board
        .lanes
        .iter()
        .find(|l| l.id == card.lane_id)
        .map(|l| l.name.as_str())
        .unwrap_or("?");
    println!("📝 {}", card.title);
    println!("   id: {}", card.id);
    println!("   board: {}", board.name);
    println!("   lane: {}", lane);
    println!("   status: {}", if card.done { "done ✓" } else { "open ○" });
    if let Some(desc) = &card.description {
        println!("   description: {}", desc);
    }
    println!("   created: {}", card.created_at);
    println!("   updated: {}", card.updated_at);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, KanbanStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = KanbanStore::at(dir.path().join("kanban.json")).expect("store open");
        (dir, store)
    }

    #[test]
    fn create_board_seeds_default_lanes() {
        let (_dir, mut store) = temp_store();
        let id = store.create_board("Sprint 1", None).expect("create board");
        assert!(!id.is_empty());
        assert_eq!(store.board_count(), 1);
        let board = store.find_board("Sprint 1").expect("board present");
        let names: Vec<&str> = board.lanes.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["Todo", "In Progress", "Done"]);
        assert_eq!(board.id, id);
    }

    #[test]
    fn create_board_rejects_duplicates_and_empty_names() {
        let (_dir, mut store) = temp_store();
        store.create_board("Sprint 1", None).expect("create board");
        assert!(store.create_board("Sprint 1", None).is_err());
        assert!(store.create_board("   ", None).is_err());
    }

    #[test]
    fn delete_board_removes_it() {
        let (_dir, mut store) = temp_store();
        store.create_board("Sprint 1", None).expect("create board");
        let name = store.delete_board("Sprint 1").expect("delete board");
        assert_eq!(name, "Sprint 1");
        assert_eq!(store.board_count(), 0);
        assert!(store.delete_board("Sprint 1").is_err());
    }

    #[test]
    fn add_card_lands_in_first_lane() {
        let (_dir, mut store) = temp_store();
        store.create_board("Sprint 1", None).expect("create board");
        let card_id = store
            .create_card("Sprint 1", "Write docs", Some("v2 docs"), None)
            .expect("create card");
        let board = store.find_board("Sprint 1").expect("board present");
        let card = board.cards.iter().find(|c| c.id == card_id).expect("card");
        assert_eq!(card.title, "Write docs");
        assert_eq!(card.description.as_deref(), Some("v2 docs"));
        assert!(!card.done);
        let lane = board
            .lanes
            .iter()
            .find(|l| l.id == card.lane_id)
            .expect("lane");
        assert_eq!(lane.name, "Todo");
    }

    #[test]
    fn move_card_updates_lane_and_done_flag() {
        let (_dir, mut store) = temp_store();
        store.create_board("Sprint 1", None).expect("create board");
        let card_id = store
            .create_card("Sprint 1", "Write docs", None, None)
            .expect("create card");

        store
            .move_card("Sprint 1", &card_id, "In Progress")
            .expect("move card");
        let board = store.find_board("Sprint 1").expect("board present");
        let card = board.cards.iter().find(|c| c.id == card_id).expect("card");
        assert!(!card.done);
        let lane = board
            .lanes
            .iter()
            .find(|l| l.id == card.lane_id)
            .expect("lane");
        assert_eq!(lane.name, "In Progress");

        // Moving into the done lane completes the card.
        store.move_card("Sprint 1", &card_id, "done").expect("move");
        let board = store.find_board("Sprint 1").expect("board present");
        let card = board.cards.iter().find(|c| c.id == card_id).expect("card");
        assert!(card.done);

        // Moving back out re-opens it.
        store.move_card("Sprint 1", &card_id, "Todo").expect("move");
        let board = store.find_board("Sprint 1").expect("board present");
        let card = board.cards.iter().find(|c| c.id == card_id).expect("card");
        assert!(!card.done);
    }

    #[test]
    fn complete_card_marks_done() {
        let (_dir, mut store) = temp_store();
        store.create_board("Sprint 1", None).expect("create board");
        let card_id = store
            .create_card("Sprint 1", "Write docs", None, None)
            .expect("create card");
        store
            .complete_card("Sprint 1", &card_id)
            .expect("complete card");
        let board = store.find_board("Sprint 1").expect("board present");
        let card = board.cards.iter().find(|c| c.id == card_id).expect("card");
        assert!(card.done);
        let lane = board
            .lanes
            .iter()
            .find(|l| l.id == card.lane_id)
            .expect("lane");
        assert!(lane.name.eq_ignore_ascii_case("done"));
    }

    #[test]
    fn create_lane_and_move_by_title() {
        let (_dir, mut store) = temp_store();
        store.create_board("Sprint 1", None).expect("create board");
        store
            .create_lane("Sprint 1", "Review")
            .expect("create lane");
        store
            .create_card("Sprint 1", "Write docs", None, Some("Review"))
            .expect("create card");
        // Resolve the card by exact title instead of id.
        store
            .move_card("Sprint 1", "Write docs", "Todo")
            .expect("move by title");
        let board = store.find_board("Sprint 1").expect("board present");
        let card = board
            .cards
            .iter()
            .find(|c| c.title == "Write docs")
            .expect("card");
        let lane = board
            .lanes
            .iter()
            .find(|l| l.id == card.lane_id)
            .expect("lane");
        assert_eq!(lane.name, "Todo");
        // Duplicate lane names (case-insensitive) are rejected.
        assert!(store.create_lane("Sprint 1", "review").is_err());
    }

    #[test]
    fn delete_card_removes_it() {
        let (_dir, mut store) = temp_store();
        store.create_board("Sprint 1", None).expect("create board");
        let card_id = store
            .create_card("Sprint 1", "Write docs", None, None)
            .expect("create card");
        let title = store
            .delete_card("Sprint 1", &card_id)
            .expect("delete card");
        assert_eq!(title, "Write docs");
        let board = store.find_board("Sprint 1").expect("board present");
        assert!(board.cards.is_empty());
        assert!(store.delete_card("Sprint 1", &card_id).is_err());
    }

    #[test]
    fn store_roundtrips_through_json() {
        let (dir, mut store) = temp_store();
        store
            .create_board("Sprint 1", Some("march"))
            .expect("create board");
        let card_id = store
            .create_card("Sprint 1", "Write docs", None, None)
            .expect("create card");
        store.save().expect("save");

        let store2 = KanbanStore::at(dir.path().join("kanban.json")).expect("reload");
        let board = store2.find_board("Sprint 1").expect("board present");
        assert_eq!(board.description.as_deref(), Some("march"));
        assert!(board.cards.iter().any(|c| c.id == card_id));
    }

    #[test]
    fn ambiguous_card_key_is_rejected() {
        let (_dir, mut store) = temp_store();
        store.create_board("Sprint 1", None).expect("create board");
        let first = store
            .create_card("Sprint 1", "Task A", None, None)
            .expect("card");
        let second = store
            .create_card("Sprint 1", "Task B", None, None)
            .expect("card");
        // Both ids start with "card-"; a shared prefix must not silently pick one.
        let prefix_len = first
            .chars()
            .zip(second.chars())
            .take_while(|(a, b)| a == b)
            .count()
            .max(1);
        assert!(store.delete_card("Sprint 1", &first[..prefix_len]).is_err());
    }
}
