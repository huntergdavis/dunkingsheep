import './fonts/fonts.css';
// Preserve the legacy cascade: library CSS, Default, variants, then component CSS.
import './themes/default.css';
import { themeVariants } from './theme-variants.js';

export const themeStyles = { './themes/default.css': true, ...themeVariants };
