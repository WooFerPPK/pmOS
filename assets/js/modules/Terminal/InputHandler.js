import { Actions } from './Actions.js';

/**
 * Manages input interactions for a terminal-like interface.
 */
export class InputHandler {
    /**
     * Constructs a new InputHandler.
     * @param {HTMLElement} inputElement - The element receiving input.
     * @param {Object} terminal - The terminal interface.
     * @param {Actions} [actions=null] - The list of supported actions.
     */
    constructor(inputElement, terminal, actions = null) {
        this.inputElement = inputElement;
        this.terminal = terminal;
        this.actions = actions || new Actions(terminal);
        this.boundListener = this.handleKeydown.bind(this); // Binding to retain this context
        this.freeInputProcessor = null;
        this.initInputListener();
    }

    /**
     * Initializes the input listener.
     */
    initInputListener() {
        this.inputElement.addEventListener('keydown', this.boundListener);
    }

    /**
     * Handles keydown events for the input element.
     * @param {KeyboardEvent} event - The keydown event.
     */
    handleKeydown(event) {
        // Prevent input if terminal output is running.
        if (this.terminal.isOutputRunning) {
            event.preventDefault();
            return;
        }

        // Process 'Enter' key press
        if (event.key === 'Enter') {
            event.preventDefault();
            const command = event.target.textContent.trim();

            if (this.freeInputProcessor) {
                // In free input mode, process the entire string
                this.terminal.outputHandler.displayOutput(command, this.processFreeInput(command));
            } else {
                // Process the input as a command
                this.terminal.addCommandToHistory(command);
                this.processCommand(command);
            }

            // Clear the input after processing
            event.target.textContent = '';

        // Process 'ArrowUp' key press for command history navigation
        } else if (event.key === 'ArrowUp') {
            if (this.terminal.terminalState.historyPosition > 0) {
                this.terminal.terminalState.historyPosition--;
                this.inputElement.textContent = this.terminal.terminalState.commandHistory[this.terminal.terminalState.historyPosition];
            }
            event.preventDefault();

        // Process 'ArrowDown' key press for command history navigation
        } else if (event.key === 'ArrowDown') {
            if (this.terminal.terminalState.historyPosition < this.terminal.terminalState.commandHistory.length - 1) {
                this.terminal.terminalState.historyPosition++;
                this.inputElement.textContent = this.terminal.terminalState.commandHistory[this.terminal.terminalState.historyPosition];
            } else {
                this.terminal.terminalState.historyPosition = this.terminal.terminalState.commandHistory.length;
                this.inputElement.textContent = '';
            }
            event.preventDefault();

        // Process 'Ctrl + L' key combination to clear the terminal
        } else if (event.ctrlKey && event.key === 'l') {
            event.preventDefault();
            this.processCommand('clear');
        }
    }

    /**
     * Processes a command.
     * @param {string} command - The command string.
     */
    processCommand(command) {
        const action = this.actions[command];
        if (action) {
            const result = action.run();
            if (this.terminal) {
                if (command !== 'clear') {
                    this.terminal.outputHandler.displayOutput(command, result, true);
                }
            } else {
                return;
            }
        } else {
            if (command) {
                this.terminal.outputHandler.displayOutput(command, `${command}: command not found`);
            } else {
                this.terminal.outputHandler.displayOutput('', '');
            }

        }
    }

    /**
     * Processes input when in free input mode.
     * @param {string} input - The input string.
     * @returns {string} - Processed result or error message.
     */
    processFreeInput(input) {
        if (input === 'exit') {
            this.freeInputProcessor = null; // Exit free input mode
            return 'Exited application. You can now use regular terminal commands.';
        }

        // Attempt to process input as a mathematical expression
        try {
            let result = this.freeInputProcessor(input);
            return result.toString();
        } catch (error) {
            return 'Invalid expression.';
        }
    }

    /**
     * Cleans up and removes event listeners.
     */
    destroy() {
        this.inputElement.removeEventListener('keydown', this.boundListener);
        this.terminal = null;
        this.actions = null;
    }
}
