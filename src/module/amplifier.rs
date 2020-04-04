use crate::engine::{Sample, ZERO_BUFFER_STEREO, ONE_BUFFER_MONO};
use crate::module::{Module, LineType};

use mixlab_protocol::AmplifierParams;

#[derive(Debug)]
pub struct Amplifier {
    params: AmplifierParams
}

impl Module for Amplifier {
    type Params = AmplifierParams;
    type Indication = ();

    fn create(params: Self::Params) -> (Self, Self::Indication) {
        (Amplifier {params}, ())
    }

    fn params(&self) -> Self::Params {
        self.params.clone()
    }

    fn update(&mut self, params: Self::Params) -> Option<Self::Indication> {
        self.params = params;
        None
    }

    fn run_tick(&mut self, _t: u64, inputs: &[Option<&[Sample]>], outputs: &mut [&mut [Sample]]) -> Option<Self::Indication> {
        let AmplifierParams {mod_depth, amplitude} = self.params;

        let input = &inputs[0].unwrap_or(&ZERO_BUFFER_STEREO);
        let mod_input = &inputs[1].unwrap_or(&ONE_BUFFER_MONO);
        let output = &mut outputs[0];

        let len = input.len();

        for i in 0..len {
            // mod input is a mono channel and so half the length:
            let mod_value = mod_input[i / 2];

            output[i] = input[i] * depth(mod_value, mod_depth) * amplitude;
        }

        None
    }

    fn inputs(&self) -> &[LineType] {
        &[LineType::Stereo, LineType::Mono]
    }

    fn outputs(&self) -> &[LineType] {
        &[LineType::Stereo]
    }
}

pub fn depth(value: f32, depth: f32) -> f32 {
    1.0 - depth + depth * value
}