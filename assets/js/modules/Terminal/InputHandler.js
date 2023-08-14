import { Actions } from './Actions.js';

export class InputHandler {
    constructor(inputElement, terminal, actions = null) {
        this.inputElement = inputElement;
        this.terminal = terminal;
        this.actions = actions || new Actions(terminal);
        this.boundListener = this.handleKeydown.bind(this);
        this.initInputListener();
    }
    initInputListener() {
        this.inputElement.addEventListener('keydown', this.boundListener);
    }

    handleKeydown(event) {
        if (this.terminal.isOutputRunning) {
            event.preventDefault();
            return;
        }

        if (event.key === 'Enter') {
            event.preventDefault();
            const command = event.target.textContent.trim();
            this.terminal.addCommandToHistory(command);
            this.processCommand(command);
            event.target.textContent = '';

        } else if (event.key === 'ArrowUp') {
            if (this.terminal.terminalState.historyPosition > 0) {
                this.terminal.terminalState.historyPosition--;
                this.inputElement.textContent = this.terminal.terminalState.commandHistory[this.terminal.terminalState.historyPosition];
            }
            event.preventDefault();

        } else if (event.key === 'ArrowDown') {
            if (this.terminal.terminalState.historyPosition < this.terminal.terminalState.commandHistory.length - 1) {
                this.terminal.terminalState.historyPosition++;
                this.inputElement.textContent = this.terminal.terminalState.commandHistory[this.terminal.terminalState.historyPosition];
            } else {
                this.terminal.terminalState.historyPosition = this.terminal.terminalState.commandHistory.length;
                this.inputElement.textContent = '';
            }
            event.preventDefault();

        } else if (event.ctrlKey && event.key === 'l') {
            event.preventDefault();
            this.processCommand('clear');
        }
    }

    processCommand(command) {
        const action = this.actions[command];
        if (action) {
            const result = action.run();
            if (this.terminal) {
                if (command !== "clear") {
                    this.terminal.outputHandler.displayOutput(command, result, true);
                }
            } else {
                return;
            }
        } else {
            this.terminal.outputHandler.displayOutput(command, `${command}: command not found`);
        }
    }

    destroy() {
        this.inputElement.removeEventListener('keydown', this.boundListener);
        // this.inputElement = null;
        this.terminal = null;
        this.actions = null;
    }
}
