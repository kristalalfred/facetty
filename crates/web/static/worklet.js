// Claude: the microphone's samples go to the page as they arrive, mixed down
// to mono. The playback side keeps about 60 ms queued and asks the page for
// another 20 ms whenever it runs below that.

class Capture extends AudioWorkletProcessor {
  process(inputs) {
    const channels = inputs[0];
    if (channels.length > 0) {
      const mono = new Float32Array(channels[0].length);
      for (const channel of channels) {
        for (let i = 0; i < mono.length; i++) mono[i] += channel[i] / channels.length;
      }
      this.port.postMessage(mono, [mono.buffer]);
    }
    return true;
  }
}

const QUEUED = 0.06;
const FRAME = 0.02;

class Playback extends AudioWorkletProcessor {
  constructor() {
    super();
    this.chunks = [];
    this.offset = 0;
    this.queued = 0;
    this.asked = 0;
    this.port.onmessage = (event) => {
      this.chunks.push(event.data);
      this.queued += event.data.length;
      this.asked = Math.max(0, this.asked - 1);
    };
  }

  process(_, outputs) {
    const out = outputs[0][0];
    let written = 0;
    while (written < out.length && this.chunks.length > 0) {
      const chunk = this.chunks[0];
      const n = Math.min(out.length - written, chunk.length - this.offset);
      out.set(chunk.subarray(this.offset, this.offset + n), written);
      written += n;
      this.offset += n;
      this.queued -= n;
      if (this.offset === chunk.length) {
        this.chunks.shift();
        this.offset = 0;
      }
    }
    while (this.queued + this.asked * FRAME * sampleRate < QUEUED * sampleRate) {
      this.port.postMessage(0);
      this.asked++;
    }
    return true;
  }
}

registerProcessor("facetty-capture", Capture);
registerProcessor("facetty-playback", Playback);
