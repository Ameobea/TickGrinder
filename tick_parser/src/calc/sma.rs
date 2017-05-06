use std::collections::VecDeque;

use algobot_util::trading::tick::Tick;

//Calculate weighted average of all ticks within period seconds
//pop ticks off the front after they leave the period

pub struct SMAList {
    pub smas: Vec<SimpleMovingAverage>
}

impl SMAList {
    pub fn new() -> SMAList {
        SMAList {
            smas: Vec::new()
        }
    }

    pub fn push_all(&mut self, t: Tick) {
        let smas = &mut self.smas;
        for mut sma in smas {
            let sma = &mut sma;
            sma.push(t);
        }
    }

    pub fn add(&mut self, period: usize) {
        self.smas.push(SimpleMovingAverage::new(period));
    }

    pub fn remove(&mut self, period: usize) {
        for i in 0..self.smas.len() {
            if self.smas[i].period == period {
                self.smas.remove(i);
                return
            }
        }
        println!("No SMA with period {} currently tracked", period);
    }
}

pub struct SimpleMovingAverage {
    pub period: usize,
    pub ticks: VecDeque<Tick>,
    // indicates if an out-of-range tick exists in the front element
    ref_tick: Tick,
}

impl SimpleMovingAverage {
    pub fn new(period: usize) -> SimpleMovingAverage {
        SimpleMovingAverage {
            period: period,
            ticks: VecDeque::new(),
            ref_tick: Tick::null(),
        }
    }

    // trims out of range ticks from the front of the queue
    // returns the last out-of-range tick removed
    fn trim(&mut self) -> Tick {
        let mut t: Tick = Tick::null();
        while self.is_overflown() {
            t = self.ticks.pop_front().unwrap();
        }
        t
    }

    fn average(&self) -> usize {
        let mut p_sum = 0; // sum of prices
        let mut t_sum = 0; // sum of time
        let mut iter = self.ticks.iter();
        iter.next(); // skip first value since there's no time difference to compute
        let mut last_tick = self.ticks.front().unwrap();
        // loop over ticks, oldest to newest
        for t in iter {
            let t_diff = t.timestamp - last_tick.timestamp;
            p_sum += last_tick.mid() * t_diff;
            t_sum += t_diff;
            last_tick = t;
        }

        // if there is a previous value to take into account
        if self.ref_tick.bid != 0 {
            let old_time = self.period - t_sum;
            p_sum += old_time * self.ref_tick.mid();
            t_sum = self.period;
        }

        p_sum / t_sum
    }

    fn is_overflown(&self) -> bool {
        // time between newest tick and reference tick
        let diff = self.ticks.back().unwrap().timestamp - self.ticks.front().unwrap().timestamp;
        diff >= self.period
    }

    // Add a new tick to be averaged.
    pub fn push(&mut self, t: Tick) -> usize {
        // open new section so we're not double-borrowing self.ticks
        {
            let last_tick: Option<&Tick> = self.ticks.back();
            if last_tick.is_some() {
                assert!(t.timestamp > last_tick.unwrap().timestamp, "Out-of-order ticks sent to SMA!
                    timestamps: {:?}, {:?}", last_tick.unwrap().timestamp, t.timestamp);
            }
        }
        self.ticks.push_back(t);

        if self.is_overflown() {
            self.ref_tick = self.trim();
        }

        if self.ticks.len() == 1 {
            return self.ticks.front().unwrap().mid()
        }

        self.average()
    }

    pub fn average_tick(&self) -> Tick {
        let mut bid_sum = 0;
        let mut ask_sum = 0;
        let mut t_sum = 0; // sum of time
        let mut iter = self.ticks.iter();
        iter.next(); // skip first value since there's no time difference to compute
        let mut last_tick = self.ticks.front().unwrap();
        // loop over ticks, oldest to newest
        for t in iter {
            let t_diff = (t.timestamp - last_tick.timestamp) as usize;
            bid_sum += last_tick.bid * t_diff;
            ask_sum += last_tick.ask * t_diff;
            t_sum += t_diff;
            last_tick = t;
        }

        // if there is a previous value to take into account
        if self.ref_tick.bid != 0 {
            let old_time = self.period - t_sum;
            bid_sum += old_time * self.ref_tick.bid;
            ask_sum += old_time * self.ref_tick.ask;
            t_sum = self.period;
        }

        Tick { bid: bid_sum / t_sum, ask: ask_sum / t_sum, timestamp: (*self.ticks.back().unwrap()).timestamp }
    }

    pub fn push_tick(&mut self, t: Tick) -> Tick {
        // open new section so we're not double-borrowing self.ticks
        {
            let last_tick: Option<&Tick> = self.ticks.back();
            if last_tick.is_some() {
                assert!(t.timestamp > last_tick.unwrap().timestamp, "Out-of-order ticks sent to SMA!
                    timestamps: {:?}, {:?}", last_tick.unwrap().timestamp, t.timestamp);
            }
        }
        self.ticks.push_back(t);

        if self.is_overflown() {
            self.ref_tick = self.trim();
        }

        if self.ticks.len() == 1 {
            return *self.ticks.front().unwrap()
        }

        self.average_tick()
    }
}