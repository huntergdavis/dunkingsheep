// The Projects "Model" cell (t4473). A model is shown only with where it came from and how
// old the sample is; a missing sample, or one older than 15 minutes, is "unknown" with the
// reason. An old or guessed model is never shown as the lane's current one.
export const MODEL_FRESH_S = 15 * 60;

function seenText(seconds) {
  if (seconds < 60) return 'seen less than 1 min ago';
  return `seen ${Math.floor(seconds / 60)} min ago`;
}

export function laneModel(model) {
  const value = typeof model?.value === 'string' && model.value ? model.value : null;
  const age = Number.isFinite(model?.age_s) && model.age_s >= 0 ? model.age_s : null;
  if (value === null) {
    return { known: false, text: 'unknown', detail: model?.reason || 'no model sample' };
  }
  if (age === null) {
    return { known: false, text: 'unknown', detail: 'the sample has no age' };
  }
  if (age > MODEL_FRESH_S) {
    return { known: false, text: 'unknown', detail: 'the last sample is older than 15 minutes' };
  }
  const effort = typeof model.effort === 'string' && model.effort ? ` ${model.effort}` : '';
  return {
    known: true,
    text: `${value}${effort}`,
    detail: `${seenText(age)}, ${model.source || 'source not recorded'}`,
  };
}
