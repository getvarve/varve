/** One entry of a context menu. */
export interface MenuItem {
  id: string;
  label: string;
  danger?: boolean;
  disabled?: boolean;
  separatorBefore?: boolean;
  /** Entries of a submenu this entry opens (one level deep); choosing the entry itself does nothing. */
  children?: MenuLeaf[];
}

/** An entry of a submenu: no further nesting. */
export type MenuLeaf = Omit<MenuItem, 'children'>;
