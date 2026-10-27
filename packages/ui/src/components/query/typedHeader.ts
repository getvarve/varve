import type { IHeaderParams } from 'ag-grid-community';

/**
 * Inner header of a result column: the column name with its type name
 * below in muted mono text. A plain grid component (no framework), so it
 * stays cheap for wide results; text is set with `textContent`.
 */
export class TypedHeader {
  private gui: HTMLElement | null = null;

  init(params: IHeaderParams & { typeName?: string }): void {
    const root = document.createElement('span');
    root.className = 'typed-header';
    const name = document.createElement('span');
    name.className = 'typed-header-name';
    name.textContent = params.displayName;
    root.append(name);
    if (params.typeName) {
      const type = document.createElement('span');
      type.className = 'typed-header-type';
      type.textContent = params.typeName;
      root.append(type);
    }
    this.gui = root;
  }

  getGui(): HTMLElement {
    return this.gui ?? document.createElement('span');
  }

  refresh(): boolean {
    return false;
  }
}
