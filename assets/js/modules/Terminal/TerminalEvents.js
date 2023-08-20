/**
 * Provides an event management system for the Terminal. 
 * Allows registration, deregistration, and emission of custom events.
 * Also provides handling for the Ctrl+C key press event.
  * @class
 */
export class TerminalEvents {
    constructor() {
        // Store registered event listeners.
        this.eventListeners = {};

        // Reference to the bound Ctrl+C event handler for later removal.
        this.boundHandleCtrlC = null;
    }

    /**
     * Registers a listener for a specific event.
     * @param {string} event - The event name.
     * @param {Function} listener - The callback function to execute when the event is emitted.
     */
    on(event, listener) {
        if (!this.eventListeners[event]) {
            this.eventListeners[event] = [];
        }
        this.eventListeners[event].push(listener);
    }

    /**
     * Removes a listener from a specific event.
     * @param {string} event - The event name.
     * @param {Function} listener - The callback function to remove.
     */
    off(event, listener) {
        if (!this.eventListeners[event]) return;
        const index = this.eventListeners[event].indexOf(listener);
        if (index !== -1) {
            this.eventListeners[event].splice(index, 1);
        }
    }

    /**
     * Emits an event, executing all registered listeners for that event.
     * @param {string} event - The event name.
     * @param {*} data - The data to be passed to the listeners.
     */
    emit(event, data) {
        if (!this.eventListeners[event]) return;
        for (let listener of this.eventListeners[event]) {
            listener(data);
        }
    }

    /**
     * Initializes the Ctrl+C keydown event listener.
     * @param {OutputHandler} outputHandler - The handler to interrupt output when Ctrl+C is pressed.
     */
    initCtrlCListener(outputHandler) {
        this.boundHandleCtrlC = this.handleCtrlC.bind(this, outputHandler);
        document.addEventListener('keydown', this.boundHandleCtrlC);
    }

    /**
     * Removes the Ctrl+C keydown event listener.
     */
    removeCtrlCListener() {
        if (this.boundHandleCtrlC) {
            document.removeEventListener('keydown', this.boundHandleCtrlC);
            this.boundHandleCtrlC = null;
        }
    }

    /**
     * Handles the Ctrl+C keydown event, interrupts output if pressed.
     * @param {OutputHandler} outputHandler - The handler to interrupt output.
     * @param {KeyboardEvent} event - The keydown event.
     */
    handleCtrlC(outputHandler, event) {
        if (this.isCtrlCPressed(event)) {
            event.preventDefault();
            outputHandler.interruptOutput();
        }
    }

    /**
     * Checks if the Ctrl+C keys were pressed together.
     * @param {KeyboardEvent} event - The keydown event.
     * @returns {boolean} - True if Ctrl+C was pressed, otherwise false.
     */
    isCtrlCPressed(event) {
        return event.ctrlKey && event.key === 'c';
    }
    
}