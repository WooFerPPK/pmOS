export default class WindowManager {
    constructor(observable) {
        // Subscribe WindowManager to specific topics/events
        this.observable = observable;
        this.observable.subscribe('windowClosed', this);

        // Keep track of all open windows
        this.windows = [];

        // Keep track of the window currently in focus
        this.focusedWindow = null;
    }

    update(message) {
        if (message) {
            if (this.windows.includes(message)) {
                this.removeWindow(message);
            }
        }
    }

    // Add a window to the manager
    addWindow(win) {
        this.windows.push(win);

        // The newly added window will be set to focus by default
        this.setFocusedWindow(win);
    }

    // Remove a window from the manager
    removeWindow(win) {
        if (this.focusedWindow === win) {
            this.focusedWindow = null;
        }

        win.remove();
        this.windows = this.windows.filter(window => window !== win);
    }

    // Get the current window in focus
    getFocusedWindow() {
        return this.focusedWindow;
    }

    // Set a window to be in focus
    setFocusedWindow(win) {
        if (!win) {
            console.warn('Trying to focus a non-existent window.');
            return;
        }
        
        if (this.focusedWindow) {
            this.focusedWindow.style.opacity = '0.7';
        }
        
        if (this.windows.includes(win)) {
            this.focusedWindow = win;
            win.style.opacity = '1';
        } else {
            console.warn(`Trying to focus a window that isn't managed.`);
        }
    }
    // Get all open windows
    getOpenWindows() {
        return this.windows;
    }

    // Get the count of open windows
    getWindowCount() {
        return this.windows.length;
    }
}