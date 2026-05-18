use nih_plug::prelude::*;
use streaming_simulator::StreamingSimulator;

fn main() {
    nih_export_standalone::<StreamingSimulator>();
}
