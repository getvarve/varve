import type { Directive } from 'vue';

/**
 * `v-focus`: focuses the element when it is mounted. Inline editors are
 * inserted with `v-if`; the HTML `autofocus` attribute is only honoured once
 * per document, so it cannot be relied on for them.
 */
export const vFocus: Directive<HTMLElement> = {
  mounted: (el) => el.focus(),
};
