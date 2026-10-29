import test from 'node:test';
import assert from 'node:assert/strict';
import { profileKey, profilePlan, toggleProfile } from './character-editor-model.js';

const characters = [{ file: 'CHRDATA1.SAV', profiles: [
  { validation_profile: 'strength', eligible: true, eligible_after_application: ['level'] },
  { validation_profile: 'level', eligible: false, eligible_after_application: [] },
] }];
const strength = profileKey('CHRDATA1.SAV', 'strength');
const level = profileKey('CHRDATA1.SAV', 'level');

test('level cannot be selected without its matching predecessor', () => {
  assert.deepEqual([...toggleProfile(characters, new Set(), level)], []);
  const selected = toggleProfile(characters, new Set([strength]), level);
  assert.deepEqual(profilePlan(characters, selected).choices.map(p => p.validation_profile), ['strength', 'level']);
  assert.deepEqual([...toggleProfile(characters, selected, strength)], []);
});

test('unknown record profiles remain unavailable regardless of selected names', () => {
  const unknown = [{ ...characters[0], profiles: characters[0].profiles.map(p => ({ ...p, eligible: false })) }];
  assert.deepEqual(profilePlan(unknown, new Set([strength, level])).choices, []);
});
