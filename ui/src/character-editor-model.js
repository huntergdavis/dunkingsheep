export const profileKey = (file, profile) => `${file}:${profile}`;

// Only a selected, available predecessor can unlock its next reference profile.
export function profilePlan(characters, selected) {
  const available = new Set();
  const pending = [];
  for (const character of characters) {
    for (const profile of character.profiles) {
      const key = profileKey(character.file, profile.validation_profile);
      if (profile.eligible) available.add(key);
      if (selected.has(key)) pending.push({ character, profile, key });
    }
  }
  const choices = [];
  let changed = true;
  while (changed) {
    changed = false;
    for (let i = pending.length - 1; i >= 0; i--) {
      const { character, profile, key } = pending[i];
      if (!available.has(key)) continue;
      choices.push({ file: character.file, validation_profile: profile.validation_profile });
      for (const next of profile.eligible_after_application) available.add(profileKey(character.file, next));
      pending.splice(i, 1);
      changed = true;
    }
  }
  return { available, choices };
}

export function toggleProfile(characters, selected, key) {
  const next = new Set(selected);
  if (next.has(key)) next.delete(key); else next.add(key);
  const { choices } = profilePlan(characters, next);
  return new Set(choices.map(p => profileKey(p.file, p.validation_profile)));
}
