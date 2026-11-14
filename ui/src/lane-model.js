// The Projects "Model" cell (t4473). The cell says only what is known and how:
// - a model the lane was LAUNCHED with (a launch flag, or its process environment) reads
//   "launched as <model>", with the source and the launch or read time;
// - "seen N min ago" is used only for a value read from the running lane itself
//   (observed === true), and only while that reading is at most 15 minutes old;
// - anything else is "unknown" with the reason. Nothing is guessed.
export const MODEL_FRESH_S = 15 * 60;

function agoText(seconds) {
  if (seconds < 60) return 'less than 1 min ago';
  if (seconds < 3600) return `${Math.floor(seconds / 60)} min ago`;
  if (seconds < 48 * 3600) return `${Math.floor(seconds / 3600)} h ago`;
  return `${Math.floor(seconds / 86400)} d ago`;
}

export function laneModel(model) {
  const unknown = detail => ({ known: false, text: 'unknown', detail });
  const value = typeof model?.value === 'string' && model.value ? model.value : null;
  if (value === null) return unknown(model?.reason || 'no model information');
  const source = typeof model.source === 'string' && model.source ? model.source : null;
  if (source === null) return unknown('the model has no recorded source');
  const age = Number.isFinite(model.age_s) && model.age_s >= 0 ? model.age_s : null;
  const effort = typeof model.effort === 'string' && model.effort ? ` ${model.effort}` : '';
  if (model.observed === true) {
    if (age === null) return unknown('the reading has no age');
    if (age > MODEL_FRESH_S) return unknown('the last reading is older than 15 minutes');
    return { known: true, text: `${value}${effort}`, detail: `seen ${agoText(age)}, ${source}` };
  }
  if (source === 'launch flag') {
    return {
      known: true,
      text: `launched as ${value}${effort}`,
      detail: age === null ? 'launch flag, launch time not recorded' : `launch flag, launched ${agoText(age)}`,
    };
  }
  if (source === 'process environment') {
    if (age === null) return unknown('the environment reading has no age');
    if (age > MODEL_FRESH_S) return unknown('the last environment reading is older than 15 minutes');
    return { known: true, text: `launched as ${value}${effort}`, detail: `process environment, read ${agoText(age)}` };
  }
  return unknown(`unrecognised model source: ${source}`);
}
