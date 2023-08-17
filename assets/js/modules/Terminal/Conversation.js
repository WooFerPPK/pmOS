import { PromptHandler } from './PromptHandler.js';

export class Conversation {
    constructor(initialQuestion, terminal, prompt) {
        this.currentQuestion = initialQuestion;
        this.terminal = terminal;
        this.prompt = prompt;
    }

    start() {
        const promptHandler = new PromptHandler(this.terminal, this.currentQuestion.prompt, this.currentQuestion.responses);
        promptHandler.handle();
    }

    end() {
        this.terminal.inputHandler.initInputListener();
    }

    handleResponse(response) {
        const action = this.currentQuestion.responses[response];
        if (action) {
            action.call(this); 
            this.terminal.inputHandler.endConversation();
        } else {
            this.terminal.outputHandler.displayOutput(response, 'You must answer with one of the provided options.');
            this.start();
        }
    }
}