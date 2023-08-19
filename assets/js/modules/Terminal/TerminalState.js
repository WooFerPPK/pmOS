/**
 * Manages the state of the Terminal, specifically around output status 
 * and command history.
 * 
 * @class
 */
export class TerminalState {
    constructor() {
        // Flag indicating if the output is currently running.
        this._isOutputRunning = false;

        // Keeps track of commands that were executed.
        this.commandHistory = [];

        // Position in the command history. Useful for features like navigating through history.
        this.historyPosition = -1;
    }

    /**
     * Setter for the output running status.
     * 
     * @param {boolean} value - True if output is running, false otherwise.
     */
    set isOutputRunning(value) {
        this._isOutputRunning = value;
    }

    /**
     * Getter for the output running status.
     * 
     * @returns {boolean} - True if output is running, false otherwise.
     */
    get isOutputRunning() {
        return this._isOutputRunning;
    }

    /**
     * Adds a command to the terminal's history.
     * 
     * @param {string} command - The command string to add to the history.
     */
    addCommandToHistory(command) {
        this.commandHistory.push(command);

        // Update the history position to the latest command.
        this.historyPosition = this.commandHistory.length;
    }

    /**
     * Fetches the last command entered in the terminal.
     * 
     * @returns {string} - The last command from the terminal history or an empty string if no commands are stored.
     */
    lastCommand() {
        if (this.commandHistory.length !== 0) {
            return this.commandHistory[this.commandHistory.length - 1];
        } else {
            return '';
        }
    }
}
