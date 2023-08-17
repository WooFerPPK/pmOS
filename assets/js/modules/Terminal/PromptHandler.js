import { InputHandler } from './InputHandler.js';

export class PromptHandler {
    constructor(terminal, prompt, actions) {
        this.terminal = terminal;
        this.prompt = prompt;
        this.actions = actions;
    }

    handle() {
        if (this.prompt) {
            this.terminal.outputHandler.displayOutput(this.terminal.lastCommand(), this.prompt);
        }
        if (this.terminal.inputHandler) {
            this.terminal.inputHandler.destroy();
        }
        this.terminal.inputHandler = new InputHandler(this.terminal.terminalUI.terminalElement.querySelector('#input'), this.terminal, this.actions);
    }
}