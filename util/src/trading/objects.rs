//! Holds definitions of the internal representations of trading objects and
//! abstractions for messages sent and received to brokers.

use std::collections::HashMap;

use uuid::Uuid;

use trading::trading_condition::{TradingAction};
use trading::broker::*;

/// An account
#[derive(Clone, Debug)]
pub struct Account {
    pub uuid: Uuid,
    pub ledger: Ledger,
    pub live: bool, // false if a demo account
}

/// Any action that the platform can take using the broker
#[derive(Clone, Debug, PartialEq)]
pub enum BrokerAction {
    TradingAction{ account_uuid: Uuid, action: TradingAction },
    /// Returns a Pong with the timestamp the broker received the message
    Ping,
    Disconnect,
}

/// A response from a broker indicating the result of an action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrokerMessage {
    Success,
    Failure,
    Notice,
    PositionOpened{
        position_id: Uuid,
        position: Position,
        timestamp: u64
    },
    PositionClosed{
        position_id: Uuid,
        position: Position,
        reason: PositionClosureReason,
        timestamp: u64,
    },
    PositionModified{
        position_id: Uuid,
        position: Position,
        timestamp: u64,
    },
    Pong{time_received: u64},
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BrokerError {
    Message{message: String},
    Unimplemented{message: String}, // the broker under the wrapper can't do what you asked it
    InsufficientBuyingPower,
    NoSuchPosition,
    NoSuchAccount,
    NoSuchSymbol,
    InvalidModificationAmount,
    NoDataAvailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PositionClosureReason {
    StopLoss,
    TakeProfit,
    MarginCall,
    Expired,
    FillOrKill,
    MarketClose,
}

/// The platform's internal representation of the current state of an account.
/// Contains information about past trades as well as current positions.
#[derive(Clone, Debug)]
pub struct Ledger {
    pub balance: usize,
    pub pending_positions: HashMap<Uuid, Position>,
    pub open_positions: HashMap<Uuid, Position>,
    pub closed_positions: HashMap<Uuid, Position>,
}

impl Ledger {
    pub fn new(starting_balance: usize) -> Ledger {
        Ledger {
            balance: starting_balance,
            pending_positions: HashMap::new(),
            open_positions: HashMap::new(),
            closed_positions: HashMap::new(),
        }
    }

    /// Attempts to open a pending position in the ledger with the supplied position.
    pub fn place_order(&mut self, pos: Position, margin_requirement: usize) -> BrokerResult {
        if margin_requirement > self.balance {
            return Err(BrokerError::InsufficientBuyingPower)
        }
        self.balance -= margin_requirement;
        unimplemented!(); // TODO
    }

    /// Opens the supplied position in the ledger.
    pub fn open_position(&mut self, uuid: Uuid, pos: Position) -> BrokerResult {
        // we assume that the supplied execution time is valid here
        let execution_time = pos.execution_time.unwrap();
        if pos.execution_price.is_none() {
            return Err(BrokerError::Message{
                message: "The supplied position does not have an entry price.".to_string()
            })
        }

        self.open_positions.insert(uuid, pos.clone());
        Ok(BrokerMessage::PositionOpened{
            position_id: uuid,
            position: pos,
            timestamp: execution_time,
        })
    }

    /// Completely closes the specified condition at the given price, crediting the account the
    /// funds yielded.  Timestamp is the time the order was submitted + any simulated delays.
    pub fn close_position(
        &mut self, uuid: Uuid, position_value: usize, timestamp: u64, reason: PositionClosureReason
    ) -> BrokerResult {
        let pos_opt = self.open_positions.remove(&uuid);
        if pos_opt.is_none() {
            return Err(BrokerError::NoSuchPosition)
        }
        self.balance += position_value;

        Ok(BrokerMessage::PositionClosed{
            position: pos_opt.unwrap(),
            position_id: uuid,
            reason: reason,
            timestamp: timestamp,
        })
    }

    /// Increases or decreases the size of the specified position by the given amount.  Returns errors
    /// if the account doesn't have enough buying power to execute the action or if a position with
    /// the specified UUID doesn't exist.
    pub fn resize_position(&mut self, uuid: Uuid, units: isize, modification_cost: usize, timestamp: u64) -> BrokerResult {
        let mut pos = self.open_positions.remove(&uuid)
            .expect("No position found with that UUID; should have caught this earlier.");

        let unit_diff = units + (pos.size as isize);
        if unit_diff < 0 {
            return Err(BrokerError::InvalidModificationAmount);
        } else if unit_diff == 0 {
            return self.close_position(uuid, modification_cost, timestamp, PositionClosureReason::MarketClose);
        }

        if self.balance < modification_cost {
            return Err(BrokerError::InsufficientBuyingPower);
        }

        // everything seems to be in order, so do the modification
        pos.size = ((pos.size as isize) + units) as usize;
        self.balance -= modification_cost;
        self.open_positions.insert(uuid, pos.clone());

        Ok(BrokerMessage::PositionModified{
            position: pos,
            position_id: uuid,
            timestamp: timestamp,
        })
    }

    pub fn modify_position(&mut self, pos_uuid: Uuid, sl: Option<usize>, tp: Option<usize>, timestamp: u64) -> BrokerResult {
        match self.open_positions.get_mut(&pos_uuid) {
            Some(pos) => {
                pos.stop = sl;
                pos.take_profit = tp;
                Ok(BrokerMessage::PositionModified{
                    position: pos.clone(),
                    position_id: pos_uuid,
                    timestamp: timestamp,
                })
            },
            None => {
                Err(BrokerError::NoSuchPosition)
            },
        }
    }
}

/// Represents an opened, closed, or pending position on a broker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Position {
    pub creation_time: u64,
    pub symbol_id: usize,
    pub size: usize,
    pub price: Option<usize>,
    pub long: bool,
    pub stop: Option<usize>,
    pub take_profit: Option<usize>,
    /// the price the position was actually executed
    pub execution_time: Option<u64>,
    /// the price the position was actually executed at
    pub execution_price: Option<usize>,
    /// the price the position was actually closed at
    pub exit_price: Option<usize>,
    /// the time the position was actually closed
    pub exit_time: Option<u64>,
}

impl Position {
    /// Returns the price the position would execute at if the prices are at levels such that the position
    /// can open, else returns None.
    pub fn is_open_satisfied(&self, bid: usize, ask: usize) -> Option<usize> {
        // only meant to be used for pending positions
        assert_eq!(self.execution_price, None);
        // only meant for limit/entry orders
        assert!(self.price.is_some());

        if self.long && ask <= self.price.unwrap() {
            return Some(ask);
        } else if bid >= self.price.unwrap() {
            return Some(bid);
        }

        None
    }

    /// Returns the price the position would execute at if the position meets
    /// the conditions for closure and the reason for its closure, else returns None.
    #[allow(collapsible_if)]
    pub fn is_close_satisfied(&self, bid: usize, ask: usize) -> Option<(usize, PositionClosureReason)> {
        // only meant to be used for open positions
        assert!(self.execution_price.is_some());
        assert_eq!(self.exit_price, None);

        if self.long {
            if self.stop.is_some() && self.stop.unwrap() >= bid {
                return Some( (bid, PositionClosureReason::StopLoss) );
            } else if self.take_profit.is_some() && self.take_profit.unwrap() <= ask {
                return Some( (ask, PositionClosureReason::StopLoss) );
            }
        } else {
            if self.stop.is_some() && self.stop.unwrap() <= ask {
                return Some( (ask, PositionClosureReason::TakeProfit) )
            } else if self.take_profit.is_some() && self.take_profit.unwrap() >= bid {
                return Some( (bid, PositionClosureReason::TakeProfit) );
            }
        }

        None
    }
}