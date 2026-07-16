// Tracks an in-app HTML5 drag (e.g. filing a meeting into a sidebar folder)
// so global Tauri drag listeners (audio-import drop overlay) can ignore it.
let active = false;

export const setInternalDragActive = (value: boolean) => {
  active = value;
};

export const isInternalDragActive = () => active;
