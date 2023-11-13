use super::{AudioNode, ChannelConfig, ChannelConfigOptions};
use crate::context::AudioParamId;
use crate::context::{AudioContextRegistration, BaseAudioContext};
use crate::param::{AudioParam, AudioParamDescriptor};
use crate::render::{AudioProcessor, AudioRenderQuantum, RenderScope};

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};

pub struct AudioParamValues<'a> {
    values: crate::render::AudioParamValues<'a>,
    map: &'a HashMap<String, AudioParamId>,
}

impl<'a> AudioParamValues<'a> {
    #[allow(clippy::missing_panics_doc)]
    pub fn get(&'a self, name: &str) -> impl Deref<Target = [f32]> + 'a {
        let id = self.map.get(name).unwrap();
        self.values.get(id)
    }
}

pub trait AudioWorkletProcessor {
    type ProcessorOptions: Send;

    fn constructor(opts: Self::ProcessorOptions) -> Self;

    fn parameter_descriptors() -> Vec<AudioParamDescriptor>
    where
        Self: Sized,
    {
        vec![] // empty by default
    }

    fn process<'a, 'b>(
        &mut self,
        scope: &'b RenderScope,
        inputs: &'b [&'b [&'a [f32]]],
        outputs: &'b mut [&'b mut [&'a mut [f32]]],
        params: AudioParamValues<'b>,
    ) -> bool;
}

/// Options for constructing an [`AudioWorkletNode`]
// dictionary AudioWorkletNodeOptions : AudioNodeOptions {
//     unsigned long numberOfInputs = 1;
//     unsigned long numberOfOutputs = 1;
//     sequence<unsigned long> outputChannelCount;
//     record<DOMString, double> parameterData;
//     object processorOptions;
// };
#[derive(Clone, Debug)]
pub struct AudioWorkletNodeOptions<C> {
    /// This is used to initialize the value of the AudioNode numberOfInputs attribute.
    pub number_of_inputs: usize,
    /// This is used to initialize the value of the AudioNode numberOfOutputs attribute.
    pub number_of_outputs: usize,
    /// This array is used to configure the number of channels in each output.
    pub output_channel_count: Vec<usize>,
    /// This is a list of user-defined key-value pairs that are used to set the initial value of an
    /// AudioParam with the matched name in the AudioWorkletNode.
    pub parameter_data: HashMap<String, f64>,
    /// This holds any user-defined data that may be used to initialize custom properties in an
    /// AudioWorkletProcessor instance that is associated with the AudioWorkletNode.
    pub processor_options: C,
    pub channel_config: ChannelConfigOptions,
}

impl<C: Default> Default for AudioWorkletNodeOptions<C> {
    fn default() -> Self {
        Self {
            number_of_inputs: 1,
            number_of_outputs: 1,
            output_channel_count: Vec::new(),
            parameter_data: HashMap::new(),
            processor_options: C::default(),
            channel_config: ChannelConfigOptions::default(),
        }
    }
}

/// A user-defined AudioNode
pub struct AudioWorkletNode {
    registration: AudioContextRegistration,
    channel_config: ChannelConfig,
    number_of_inputs: usize,
    number_of_outputs: usize,
    audio_param_map: HashMap<String, AudioParam>,
}

impl AudioNode for AudioWorkletNode {
    fn registration(&self) -> &AudioContextRegistration {
        &self.registration
    }

    fn channel_config(&self) -> &ChannelConfig {
        &self.channel_config
    }

    fn number_of_inputs(&self) -> usize {
        self.number_of_inputs
    }

    fn number_of_outputs(&self) -> usize {
        self.number_of_outputs
    }
}

impl AudioWorkletNode {
    /// # Panics
    ///
    /// This function panics when the number of inputs and the number of outputs of the supplied
    /// options are both equal to zero.
    pub fn new<P: AudioWorkletProcessor + 'static>(
        context: &impl BaseAudioContext,
        options: AudioWorkletNodeOptions<P::ProcessorOptions>,
    ) -> Self {
        context.register(move |registration| {
            let AudioWorkletNodeOptions {
                number_of_inputs,
                number_of_outputs,
                output_channel_count,
                parameter_data,
                processor_options,
                channel_config,
            } = options;

            if number_of_inputs == 0 && number_of_outputs == 0 {
                panic!("NotSupportedError: number of inputs and outputs cannot both be zero")
            }

            let output_channel_count = if output_channel_count.is_empty() {
                if number_of_inputs == 1 && number_of_outputs == 1 {
                    vec![] // special case
                } else {
                    vec![1; number_of_outputs]
                }
            } else {
                output_channel_count
                    .iter()
                    .copied()
                    .for_each(crate::assert_valid_number_of_channels);
                if output_channel_count.len() != number_of_outputs {
                    panic!(
                        "IndexSizeError: outputChannelCount.length should equal numberOfOutputs"
                    );
                }
                output_channel_count
            };

            // Setup audio params, set initial values when supplied via parameter_data
            let mut node_param_map = HashMap::new();
            let mut processor_param_map = HashMap::new();
            for mut param_descriptor in P::parameter_descriptors() {
                let name = std::mem::take(&mut param_descriptor.name);
                let (param, proc) = context.create_audio_param(param_descriptor, &registration);
                if let Some(value) = parameter_data.get(&name) {
                    param.set_value(*value as f32); // mismatch in spec f32 vs f64
                }
                node_param_map.insert(name.clone(), param);
                processor_param_map.insert(name, proc);
            }

            let node = AudioWorkletNode {
                registration,
                channel_config: channel_config.into(),
                number_of_inputs,
                number_of_outputs,
                audio_param_map: node_param_map,
            };

            // TODO make initialization of proc nicer
            let mut proc = None;
            let mut processor_options = Some(processor_options);
            let render = AudioWorkletRenderer {
                processor: Box::new(move |s, i, o, p| {
                    if proc.is_none() {
                        let opts = processor_options.take().unwrap();
                        proc = Some(P::constructor(opts));
                    }
                    proc.as_mut().unwrap().process(s, i, o, p)
                }),
                audio_param_map: processor_param_map,
                output_channel_count,
            };

            (node, Box::new(render))
        })
    }

    pub fn parameters(&self) -> &HashMap<String, AudioParam> {
        &self.audio_param_map
    }
}

type ProcessCallback = dyn for<'a, 'b> FnMut(
    &'b RenderScope,
    &'b [&'b [&'a [f32]]],
    &'b mut [&'b mut [&'a mut [f32]]],
    AudioParamValues<'b>,
) -> bool;

struct AudioWorkletRenderer {
    processor: Box<ProcessCallback>,
    audio_param_map: HashMap<String, AudioParamId>,
    output_channel_count: Vec<usize>,
}

// SAFETY:
// The concrete AudioWorkletProcessor is instantiated inside the render thread and won't be sent
// elsewhere. TODO how to express this in safe rust? Can we remove the Send bound from
// AudioProcessor?
unsafe impl Send for AudioWorkletRenderer {}

impl AudioProcessor for AudioWorkletRenderer {
    fn process(
        &mut self,
        inputs: &[AudioRenderQuantum],
        outputs: &mut [AudioRenderQuantum],
        params: crate::render::AudioParamValues<'_>,
        scope: &RenderScope,
    ) -> bool {
        // Bear with me, to construct a &[&[&[f32]]] we first build a backing vector of all the
        // individual sample slices. Then we chop it up to get to the right sub-slice structure.
        let inputs_flat: Vec<&[f32]> = inputs
            .iter()
            .flat_map(|input| input.channels())
            .map(|input_channel| input_channel.as_ref())
            .collect();

        let mut inputs_flat = &inputs_flat[..];
        let mut inputs_grouped: Vec<&[&[f32]]> = vec![];
        for input in inputs {
            let c = input.number_of_channels();
            let (left, right) = inputs_flat.split_at(c);
            inputs_grouped.push(left);
            inputs_flat = right;
        }

        // Set the proper channel count for the outputs
        if self.output_channel_count.is_empty() {
            // special case - single input/output - inherit channel count from input
            outputs[0].set_number_of_channels(inputs[0].number_of_channels());
        } else {
            outputs
                .iter_mut()
                .zip(self.output_channel_count.iter())
                .for_each(|(output, &channel_count)| output.set_number_of_channels(channel_count));
        }

        // Create an iterator for the output channel counts without allocating, handling also the
        // case where self.output_channel_count is empty.
        let output_channel_count = self
            .output_channel_count
            .iter()
            .copied()
            .chain(std::iter::once(inputs[0].number_of_channels()));

        let mut outputs_flat: Vec<&mut [f32]> = outputs
            .iter_mut()
            .flat_map(|output| output.channels_mut())
            .map(|output_channel| output_channel.deref_mut())
            .collect();

        let mut outputs_flat = &mut outputs_flat[..];
        let mut outputs_grouped: Vec<&mut [&mut [f32]]> = vec![];
        for c in output_channel_count {
            let (left, right) = outputs_flat.split_at_mut(c);
            outputs_grouped.push(left);
            outputs_flat = right;
        }

        let param_getter = AudioParamValues {
            values: params,
            map: &self.audio_param_map,
        };

        (self.processor)(
            scope,
            &inputs_grouped[..],
            &mut outputs_grouped[..],
            param_getter,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::OfflineAudioContext;

    #[test]
    fn send_bound() {
        #[derive(Default)]
        struct MyProcessor {
            _rc: std::rc::Rc<()>, // not send
        }

        impl AudioWorkletProcessor for MyProcessor {
            type ProcessorOptions = ();

            fn constructor(_opts: Self::ProcessorOptions) -> Self {
                Self::default()
            }

            fn process<'a, 'b>(
                &mut self,
                _scope: &'b RenderScope,
                _inputs: &'b [&'b [&'a [f32]]],
                _outputs: &'b mut [&'b mut [&'a mut [f32]]],
                _params: AudioParamValues<'b>,
            ) -> bool {
                true
            }
        }

        let context = OfflineAudioContext::new(1, 128, 48000.);
        let options = AudioWorkletNodeOptions::default();
        let _worklet = AudioWorkletNode::new::<MyProcessor>(&context, options);
    }
}
