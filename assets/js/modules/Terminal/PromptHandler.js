import { InputHandler } from './InputHandler.js';

/**
 * Manages the display of prompts in the terminal and initializes the associated input handlers.
 *
 * @class
 */
export class PromptHandler {
    /**
     * Creates a new PromptHandler instance.
     *
     * @param {Terminal} terminal - The terminal instance where the prompt will be displayed.
     * @param {string} prompt - The text or question to be presented.
     * @param {Array<Function>} actions - A list of callback actions associated with the prompt's responses.
     */
    constructor(terminal, prompt, actions) {
        // Reference to the terminal instance.
        this.terminal = terminal;

        // The question or statement to be displayed.
        this.prompt = prompt;

        // Callback actions for handling the user's input.
        this.actions = actions;
    }

    /**
     * Displays the prompt in the terminal and initializes a new input handler.
     */
    handle() {
        // If there's a prompt, display it along with the last command from the terminal.
        if (this.prompt) {
            this.terminal.outputHandler.displayOutput(this.terminal.lastCommand(), this.prompt);
        }

        // If there's an existing input handler, destroy it to make way for a new one.
        if (this.terminal.inputHandler) {
            this.terminal.inputHandler.destroy();
        }

        // Create a new input handler associated with the terminal's input element.
        this.terminal.inputHandler = new InputHandler(this.terminal.terminalUI.terminalElement.querySelector('#input'), this.terminal, this.actions);
    }
}