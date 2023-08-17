
export class TerminalState {
    constructor() {
        this._isOutputRunning = false;
        this.commandHistory = [];
        this.historyPosition = -1;
    }

    set isOutputRunning(value) {
        this._isOutputRunning = value;
    }

    get isOutputRunning() {
        return this._isOutputRunning;
    }

    addCommandToHistory(command) {
        this.commandHistory.push(command);
        this.historyPosition = this.commandHistory.length;
    }

    lastCommand() {
        if (this.commandHistory.length !== 0) {
            return this.commandHistory[this.commandHistory.length - 1];
        } else {
            return '';
        }
    }
}
