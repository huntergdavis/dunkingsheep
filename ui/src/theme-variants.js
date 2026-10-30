// Keep eager variant imports in their own dependency, after the default stylesheet.
export const themeVariants = import.meta.glob(['./themes/*.css', '!./themes/default.css'], { eager: true });
