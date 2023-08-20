import { InputHandler } from './InputHandler.js';
import { OutputHandler } from './OutputHandler.js';
import { TerminalUI } from './TerminalUI.js';
import { TerminalEvents } from './TerminalEvents.js';
import { TerminalState } from './TerminalState.js';

/**
 * Represents a terminal interface.
  * @class
 */
export class Terminal {
    /**
     * Creates a new Terminal instance.
     * @param {Element} windowElement - The main DOM element for the terminal window.
     * @param {Array} interactiveWindows - A list of windows that can interact with the terminal.
     * @param {Observable} observable - An observable instance for subscribing to and emitting events.
     */
    constructor(windowElement, interactiveWindows, observable) {
        this.interactiveWindows = interactiveWindows;
        this.observable = observable;

        // Subscribe to shutdown and closed events of the window.
        this.observable.subscribe('windowShutdown', this);
        this.observable.subscribe('windowClosed', this);

        // Initialize terminal UI, events, and state management.
        this.terminalUI = new TerminalUI(windowElement, observable);
        this.terminalEvents = new TerminalEvents(this);
        this.terminalState = new TerminalState();

        // Set up output and input handlers.
        this.outputHandler = new OutputHandler(windowElement.querySelector('#output'), this);
        this.inputHandler = new InputHandler(windowElement.querySelector('#input'), this);
        
        // Initialize various event listeners.
        this.inputHandler.initInputListener();
        this.outputHandler.displayStartupMessage();
        this.terminalUI.initFocusHandler(this.inputHandler);
        this.terminalEvents.initCtrlCListener(this.outputHandler, this.terminalElement);
    }
    
    /**
     * Handle updates based on observable messages.
     * @param {string} message - The message type received from the observable.
     */
    update(message) {
        if (message === 'shutdown') {
            this.terminalUI.destroyTerminal();
        }
    }

    /**
     * Get the status of output running.
     * @returns {boolean} - Whether the output is currently running or not.
     */
    get isOutputRunning() {
        return this.terminalState.isOutputRunning;
    }

    /**
     * Set the status of output running.
     * @param {boolean} value - The status value to set.
     */
    set isOutputRunning(value) {
        this.terminalState.isOutputRunning = value;
        this.terminalEvents.emit('outputRunningChanged', value);
    }

    /**
     * Adds a command to the terminal's history.
     * @param {string} command - The command string to add to the history.
     */
    addCommandToHistory(command) {
        this.terminalState.addCommandToHistory(command);
    }

    /**
     * Fetches the last command entered in the terminal.
     * @returns {string} - The last command from the terminal history.
     */
    lastCommand() {
        return this.terminalState.lastCommand();
    }
}