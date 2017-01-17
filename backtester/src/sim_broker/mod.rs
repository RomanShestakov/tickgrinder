//! Simulated broker used for backtests.  Contains facilities for simulating trades,
//! managing balances, and reporting on statistics from previous trades.
//!
//! See README.md for more information about the specifics of the SimBroker implementation
//! and a description of its functionality.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::collections::BinaryHeap;
use std::sync::mpsc;
use std::thread;
use std::ops::{Index, IndexMut};
use std::mem;
#[allow(unused_imports)]
use test;

use futures::{Future, Sink, oneshot, Oneshot, Complete};
use futures::stream::{self, Stream, Wait};
use futures::sync::mpsc::{unbounded, channel, UnboundedReceiver, UnboundedSender, Sender, Receiver};
use uuid::Uuid;

use tickgrinder_util::trading::tick::*;
pub use tickgrinder_util::trading::broker::*;
use tickgrinder_util::trading::trading_condition::*;
use tickgrinder_util::transport::command_server::CommandServer;

mod tests;
mod helpers;
pub use self::helpers::*;
mod client;
pub use self::client::*;

/// A simulated broker that is used as the endpoint for trading activity in backtests.  This is the broker backend
/// that creates/ingests streams that interact with the client.
pub struct SimBroker {
    /// Contains all the accounts simulated by the SimBroker
    pub accounts: Accounts,
    /// A copy of the settings generated from the input HashMap
    pub settings: SimBrokerSettings,
    /// Contains the streams that yield `Tick`s for the SimBroker as well as data about the symbols and other metadata.
    symbols: Symbols,
    /// Priority queue that maintains that forms the basis of the internal ordered event loop.
    pq: SimulationQueue,
    /// Timestamp of last price update received by broker
    timestamp: u64,
    /// Receiving end of the channel over which the `SimBrokerClient` sends messages
    client_rx: Option<mpsc::Receiver<(BrokerAction, Complete<BrokerResult>)>>,
    /// A handle to the sender for the channel through which push messages are sent
    push_stream_handle: Option<Sender<BrokerResult>>,
    /// A handle to the receiver for the channel throgh which push messages are received
    push_stream_recv: Option<Receiver<BrokerResult>>,
    /// The CommandServer used for logging
    pub cs: CommandServer,
}

// .-.
unsafe impl Send for SimBroker {}

impl SimBroker {
    pub fn new(
        settings: SimBrokerSettings, cs: CommandServer, client_rx: UnboundedReceiver<(BrokerAction, Complete<BrokerResult>)>
    ) -> SimBroker {
        let mut accounts = Accounts::new();
        // create with one account with the starting balance.
        let account = Account {
            uuid: Uuid::new_v4(),
            ledger: Ledger::new(settings.starting_balance),
            live: false,
        };
        accounts.insert(Uuid::new_v4(), account);
        // TODO: Make sure that 0 is the right buffer size for this channel
        let (client_push_tx, client_push_rx) = channel::<BrokerResult>(0);
        let (mpsc_tx, mpsc_rx) = mpsc::sync_channel(0);

        // spawn a thread to block on the `client_rx` and map it into the mpsc so we can conditionally check for new values.
        // Eventually, we'll want to use a threadsafe binary heap to avoid this behind-the-scenes involved with this.
        thread::spawn(move || {
            for msg in client_rx.wait() {
                mpsc_tx.send(msg.unwrap());
            }
        });

        SimBroker {
            accounts: accounts,
            settings: settings,
            symbols: Symbols::new(cs.clone()),
            pq: SimulationQueue::new(),
            timestamp: 0,
            client_rx: Some(mpsc_rx),
            push_stream_handle: Some(client_push_tx),
            push_stream_recv: Some(client_push_rx),
            cs: cs,
        }
    }

    /// Starts the simulation process.  Ticks are read in from the inputs and processed internally into
    /// priority queue.  The source of blocking here is determined by the client.  The `Stream`s of `Tick`s
    /// that are handed off have a capacity of 1 so that the `Sender` will block until it is consumed by
    /// the client.
    ///
    /// The assumption is that the client will do all its processing and have a chance to
    /// submit `BrokerActions` to the SimBroker before more processing more ticks, thus preserving the
    /// strict ordering by timestamp of events and fully simulating asynchronous operation.
    pub fn init_sim_loop(mut self) {
        // initialize the internal queue with values from attached tickstreams
        // all tickstreams should be added by this point
        self.pq.init(&mut self.symbols);
        self.cs.debug(None, "Internal simulation queue has been initialized.");

        // continue looping while the priority queue has new events to simulate
        while let Some(item) = self.pq.pop() {
            self.timestamp = item.timestamp;
            // first check if we have any messages from the client to process into the queue
            while let Ok((action, complete,)) = self.client_rx.as_mut().unwrap().try_recv() {
                // determine how long it takes the broker to process this message internally
                let execution_delay = self.settings.get_delay(&action);
                // insert this message into the internal queue adding on processing time
                let qi = QueueItem {
                    timestamp: self.timestamp + execution_delay,
                    unit: WorkUnit::ActionComplete(complete, action),
                };
                self.pq.push(qi);
            }

            // then process the new item we took out of the queue
            match item.unit {
                // A tick arriving at the broker.  The client doesn't get to know until after network delay.
                WorkUnit::NewTick(symbol_ix, tick) => {
                    // update the price for the popped tick's symbol
                    let price = (tick.bid, tick.ask);
                    self.symbols[symbol_ix].price = price;
                    // push the ClientTick event back into the queue + network delay
                    self.pq.push(QueueItem {
                        timestamp: tick.timestamp as u64 + self.settings.ping_ns,
                        unit: WorkUnit::ClientTick(symbol_ix, tick),
                    });
                    // check to see if we have any actions to take on open positions and take them if we do
                    self.tick_positions(symbol_ix, (tick.bid, tick.ask,));
                    // push the next future tick into the queue
                    self.pq.push_next_tick(&mut self.symbols);
                },
                // A tick arriving at the client.  We now send it down the Client's channels and block
                // until it is consumed.
                WorkUnit::ClientTick(symbol_ix, tick) => {
                    // TODO: Check to see if this does a copy and if it does, fine a way to eliminate it
                    let mut inner_symbol = &mut self.symbols[symbol_ix];
                    if tick.timestamp % 100000 == 0 {
                        println!("{}", tick.timestamp);
                    }
                    // send the tick through the client stream, blocking until it is consumed by the client.
                    inner_symbol.send_client(tick);
                },
                // The moment the broker finishes processing an action and the action takes place.
                // Begins the network delay for the trip back to the client..
                WorkUnit::ActionComplete(future, action) => {
                    // process the message and re-insert the response into the queue
                    let res = self.exec_action(&action, item.timestamp);
                    // calculate when the response would be recieved by the client
                    // then re-insert the response into the queue
                    let res_time = item.timestamp + self.settings.ping_ns;
                    let item = QueueItem {
                        timestamp: res_time,
                        unit: WorkUnit::Response(future, res),
                    };
                    self.pq.push(item);
                },
                // The moment a response reaches the client.
                WorkUnit::Response(future, res) => {
                    // fulfill the future with the result
                    future.complete(res.clone());
                    // send the push message through the channel, blocking until it's consumed by the client.
                    self.push_msg(res);
                },
            }
        }

        // if we reach here, that means we've run out of ticks to simulate from the input streams.
        let ts_string = self.timestamp.to_string();
        self.cs.notice(
            Some(&ts_string),
            "All input tickstreams have ran out of ticks; internal simulation loop stopped."
        );
    }

    /// Immediately sends a message over the broker's push channel.  Should only be called from within
    /// the SimBroker's internal event handling loop since it immediately sends the message.
    fn push_msg(&mut self, msg: BrokerResult) {
        let sender = mem::replace(&mut self.push_stream_handle, None).unwrap();
        let new_sender = sender.send(msg).wait().expect("Unable to push_msg");
        mem::replace(&mut self.push_stream_handle, Some(new_sender));
    }

    /// Returns a handle with which to send push messages.  The returned handle will immediately send
    /// messages to the client so should only be used from within the internal event handling loop.
    fn get_push_handle(&self) -> Sender<Result<BrokerMessage, BrokerError>> {
        self.push_stream_handle.clone().unwrap()
    }

    /// Initializes the push stream by creating internal messengers
    fn init_stream() -> (mpsc::Sender<Result<BrokerMessage, BrokerError>>, UnboundedReceiver<BrokerResult>) {
        let (mpsc_s, mpsc_r) = mpsc::channel::<Result<BrokerMessage, BrokerError>>();
        let tup = unbounded::<BrokerResult>();
        // wrap the sender in options so we can `mem::replace` them in the loop.
        let (mut f_s, f_r) = (Some(tup.0), tup.1);

        thread::spawn(move || {
            // block until message received over a mpsc sender
            // then re-transmit them through the push stream
            for message in mpsc_r.iter() {
                match message {
                    Ok(message) => {
                        let temp_f_s = mem::replace(&mut f_s, None).unwrap();
                        let new_f_s = temp_f_s.send(Ok(message)).wait().unwrap();
                        mem::replace(&mut f_s, Some(new_f_s));
                    },
                    Err(err_msg) => {
                        let temp_f_s = mem::replace(&mut f_s, None).unwrap();
                        let new_f_s = temp_f_s.send(Err(err_msg)).wait().unwrap();
                        mem::replace(&mut f_s, Some(new_f_s));
                    },
                }
            }
            println!("After init_stream() channel conversion loop!!");
        });

        (mpsc_s, f_r)
    }

    /// Actually carries out the action of the supplied BrokerAction (simulates it being received and processed)
    /// by a remote broker) and returns the result of the action.  The provided timestamp is that of
    /// when it was received by the broker (after delays and simulated lag).
    fn exec_action(&mut self, cmd: &BrokerAction, timestamp: u64) -> BrokerResult {
        match cmd {
            &BrokerAction::Ping => {
                Ok(BrokerMessage::Pong{time_received: timestamp})
            },
            &BrokerAction::TradingAction{account_uuid, ref action} => {
                match action {
                    &TradingAction::MarketOrder{ref symbol, long, size, stop, take_profit, max_range} => {
                        self.market_open(account_uuid, symbol, long, size, stop, take_profit, max_range, timestamp)
                    },
                    &TradingAction::MarketClose{uuid, size} => {
                        self.market_close(account_uuid, uuid, size)
                    }
                    &TradingAction::LimitOrder{account, ref symbol, long, size, stop, take_profit, entry_price} => {
                        unimplemented!(); // TODO
                    },
                    &TradingAction::LimitClose{uuid, size, exit_price} => {
                        unimplemented!(); // TODO
                    },
                    // TODO: Change this to only work with open positions
                    &TradingAction::ModifyPosition{uuid, stop, take_profit, entry_price} => {
                        self.modify_position(account_uuid, uuid, stop, take_profit)
                    }
                }
            },
            &BrokerAction::Disconnect => unimplemented!(),
        }
    }

    /// Attempts to open a position at the current market price with options for settings stop loss, or take profit.
    fn market_open(
        &mut self, account_id: Uuid, symbol: &String, long: bool, size: usize, stop: Option<usize>,
        take_profit: Option<usize>, max_range: Option<f64>, timestamp: u64
    ) -> BrokerResult {
        let opt = self.get_price(symbol);
        if opt.is_none() {
            return Err(BrokerError::NoSuchSymbol)
        }
        let (bid, ask) = opt.unwrap();

        let cur_price = if long { ask } else { bid };

        let pos = Position {
            creation_time: timestamp,
            symbol: symbol.clone(),
            size: size,
            price: Some(cur_price),
            long: long,
            stop: stop,
            take_profit: take_profit,
            execution_time: Some(timestamp + self.settings.execution_delay_ns as u64),
            execution_price: Some(cur_price),
            exit_price: None,
            exit_time: None,
        };

        let open_cost = self.get_position_value(&pos)?;

        let account_ = self.accounts.entry(account_id);
        match account_ {
            Entry::Occupied(mut occ) => {
                let mut account = occ.get_mut();
                account.ledger.open_position(pos, open_cost)
            },
            Entry::Vacant(_) => {
                Err(BrokerError::NoSuchAccount)
            }
        }
    }

    /// Attempts to close part of a position at market price.
    fn market_close(&mut self, account_id: Uuid, position_uuid: Uuid, size: usize) -> BrokerResult {
        if size == 0 {
            let ts_string = self.timestamp.to_string();
            self.cs.warning(
                Some(&ts_string),
                &format!("Warning: Attempted to close 0 units of position with uuid {}", position_uuid)
            );
            // TODO: Add configuration setting to optionally return an error
        }

        let account = match self.accounts.entry(account_id) {
            Entry::Occupied(o) => o.into_mut(),
            Entry::Vacant(_) => {
                return Err(BrokerError::NoSuchAccount);
            },
        };
        let modification_cost = match account.ledger.open_positions.entry(position_uuid) {
            Entry::Occupied(o) => {
                let pos = o.get();
                let pos_value = self.get_position_value(pos)?;
                (pos_value / pos.size) * size
            },
            Entry::Vacant(_) => {
                return Err(BrokerError::NoSuchPosition);
            }
        };
        account.ledger.resize_position(position_uuid, (-1 * size as isize), modification_cost, self.timestamp)
    }

    /// Modifies the stop loss or take profit of a position.
    fn modify_position(
        &mut self, account_id: Uuid, position_uuid: Uuid, sl: Option<usize>, tp: Option<usize>
    ) -> BrokerResult {
        let account = match self.accounts.entry(account_id) {
            Entry::Occupied(o) => o.into_mut(),
            Entry::Vacant(_) => {
                return Err(BrokerError::NoSuchAccount);
            },
        };
        account.ledger.modify_position(position_uuid, sl, tp, self.timestamp)
    }

    /// Dumps the SimBroker state to a file that can be resumed later.
    fn dump_to_file(&mut self, filename: &str) {
        unimplemented!(); // TODO
    }

    /// Used for Forex exchange rate conversions.  The cost to open a position is determined
    /// by the exchange rate between the base currency and the primary currency of the pair.
    /// A decimal precision of 10 is used for all returned results.
    ///
    /// Gets the conversion rate (in pips) between the base currency of the simbroker and
    /// the supplied currency.  If the base currency is USD and AUD is provided, the exchange
    /// rate for AUD/USD will be returned.  Returns Err if we lack the data to do that.
    fn get_base_rate(&self, symbol: &str) -> Result<usize, BrokerError> {
        if !self.settings.fx {
            return Err(BrokerError::Message{
                message: String::from("Can only convert to base rate when in FX mode.")
            });
        }

        let base_currency = &self.settings.fx_base_currency;
        let base_pair = format!("{}{}", symbol, base_currency);

        let (_, ask, decimals) = if !self.symbols.contains(&base_pair) {
            // try reversing the order or the pairs
            let base_pair_reverse = format!("{}{}", base_currency, symbol);
            if !self.symbols.contains(&base_pair_reverse) {
                return Err(BrokerError::NoDataAvailable);
            } else {
                self.symbols[&base_pair_reverse].get_price()
            }
        } else {
            self.symbols[&base_pair].get_price()
        };

        Ok(convert_decimals(ask, decimals, 10))
    }

    /// Returns the worth of a position in units of base currency.
    fn get_position_value(&self, pos: &Position) -> Result<usize, BrokerError> {
        let name = &pos.symbol;
        if !self.symbols.contains(name) {
            return Err(BrokerError::NoSuchSymbol);
        }

        let sym = &self.symbols[name];
        if sym.is_fx() {
            let base_rate = self.get_base_rate(&name)?;
            Ok(pos.size * base_rate * self.settings.fx_lot_size)
        } else {
            Ok(pos.size)
        }
    }

    /// Sets the price for a symbol.  If no Symbol currently exists with that designation, a new one
    /// will be initialized with a static price.
    fn oneshot_price_set(
        &mut self, name: String, price: (usize, usize), is_fx: bool, decimal_precision: usize,
    ) {
        let (bid, ask) = price;
        if is_fx {
            assert_eq!(name.len(), 6);
        }

        // insert new entry into `self.prices` or update if one exists
        if self.symbols.contains(&name) {
            self.symbols[&name].price = price;
        } else {
            let symbol = Symbol::new_oneshot(price, is_fx, decimal_precision);
            self.symbols.add(name, symbol).expect("Unable to set oneshot price for new symbol");
        }
    }

    /// Returns a clone of an account's ledger or an error if it doesn't exist.
    pub fn get_ledger_clone(&mut self, account_uuid: Uuid) -> Result<Ledger, BrokerError> {
        match self.accounts.get(&account_uuid) {
            Some(acct) => Ok(acct.ledger.clone()),
            None => Err(BrokerError::Message{
                message: "No account exists with that UUID.".to_string()
            }),
        }
    }

    /// Called each received tick to check if any pending positions need opening or closing.
    fn tick_positions(&mut self, symbol_ix: usize, price: (usize, usize)) {
        for (acct_id, mut acct) in self.accounts.data.iter_mut() {
            let (bid, ask) = self.symbols[symbol_ix].price;
            let mut satisfied_pendings = Vec::new();

            for (pos_id, pos) in &acct.ledger.pending_positions {
                let satisfied = pos.is_open_satisfied(bid, ask);
                // market conditions have changed and this position should be opened
                if pos.symbol_id == symbol_ix && satisfied.is_some() {
                    satisfied_pendings.push( (*pos_id, satisfied) );
                }
            }

            // fill all the satisfied pending positions
            for (pos_id, price_opt) in satisfied_pendings {
                let mut pos = acct.ledger.pending_positions.remove(&pos_id).unwrap();
                pos.execution_time = Some(self.timestamp);
                pos.execution_price = price_opt;
                // TODO: Adjust account balance and stats
                acct.ledger.open_positions.insert(pos_id, pos.clone());
                // send push message with notification of fill
                let _ = self.push_handle_tx.send(
                    Ok(BrokerMessage::PositionOpened{
                        position_id: pos_id, position: pos, timestamp: self.timestamp
                    })
                );
            }

            let mut satisfied_opens = Vec::new();
            for (pos_id, pos) in &acct.ledger.open_positions {
                let satisfied = pos.is_close_satisfied(bid, ask);
                // market conditions have changed and this position should be closed
                if pos.symbol == symbol && satisfied.is_some() {
                    satisfied_opens.push( (*pos_id, satisfied) );
                }
            }

            // close all the satisfied open positions
            for (pos_id, closure) in satisfied_opens {
                let (close_price, closure_reason) = closure.unwrap();
                let mut pos = acct.ledger.pending_positions.remove(&pos_id).unwrap();
                pos.exit_time = Some(timestamp);
                pos.exit_price = Some(close_price);
                // TODO: Adjust account balance and stats
                acct.ledger.closed_positions.insert(pos_id, pos.clone());
                // send push message with notification of close
                let _ = sender_handle.send(
                    Ok(BrokerMessage::PositionClosed{
                        position_id: pos_id, position: pos, reason: closure_reason, timestamp: timestamp
                    })
                );
            }
        }
    }

    /// Registers a data source into the SimBroker.  Ticks from the supplied generator will be
    /// used to upate the SimBroker's internal prices and transmitted to connected clients.
    pub fn register_tickstream(
        &mut self, name: String, raw_tickstream: UnboundedReceiver<Tick>, is_fx: bool, decimal_precision: usize
    ) -> BrokerResult {
        // allocate space for open positions of the new symbol in `Accounts`
        self.accounts.add_symbol();
        let mut sym = Symbol::new_from_stream(raw_tickstream.boxed(), is_fx, decimal_precision);
        // get the first element out of the tickstream and set the next tick equal to it
        let first_tick = sym.next().unwrap().unwrap();
        self.cs.debug(None, &format!("Set first tick for tickstream {}: {:?}", name, &first_tick));
        sym.next_tick = Some(first_tick);
        self.symbols.add(name, sym)
    }

    /// Returns the current price for a given symbol or None if the SimBroker
    /// doensn't have a price.
    pub fn get_price(&self, name: &String) -> Option<(usize, usize)> {
        if !self.symbols.contains(name) {
            return None;
        }

        Some(self.symbols[name].price)
    }
}
