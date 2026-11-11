/** The shared tag palette, in the order primaries take it. */
const TAGS = ['violet', 'blue', 'green', 'amber', 'red', 'gray'] as const;

/**
 * The fill of the `index`th primary of a cluster, for the slot map and the
 * node list. Six primaries take the six tag colors; the next six take the
 * same colors lighter (mixed with the surface), the six after that a
 * striped variant, so neighbouring slot ranges of different primaries never
 * share a fill in clusters of up to 18 primaries.
 */
export function primaryFill(index: number): string {
  const tag = `var(--tag-${TAGS[index % TAGS.length] ?? 'gray'})`;
  const light = `color-mix(in srgb, ${tag} 55%, var(--surface))`;
  switch (Math.floor(index / TAGS.length) % 3) {
    case 0:
      return tag;
    case 1:
      return light;
    default:
      return `repeating-linear-gradient(-45deg, ${tag} 0 3px, ${light} 3px 6px)`;
  }
}
