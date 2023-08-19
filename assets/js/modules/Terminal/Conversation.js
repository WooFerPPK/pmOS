import { PromptHandler } from './PromptHandler.js';

/**
 * Represents a conversation within the terminal.
 */
export class Conversation {
    /**
     * Creates a new conversation.
     * @param {Object} initialQuestion - The initial question for the conversation.
     * @param {Object} terminal - An object representing the terminal in which the conversation occurs.
     * @param {Object} prompt - The prompt that represents user input.
     */
    constructor(initialQuestion, terminal, prompt) {
        this.currentQuestion = initialQuestion; // Store the current (initial) question
        this.terminal = terminal; // Terminal interface object
        this.prompt = prompt; // User input prompt object
    }

    /**
     * Starts the conversation by handling the prompt.
     */
    start() {
        // Create a new prompt handler with the terminal, the current question's prompt, and possible responses.
        const promptHandler = new PromptHandler(this.terminal, this.currentQuestion.prompt, this.currentQuestion.responses);
        
        // Begin the prompt handling process
        promptHandler.handle();
    }

    /**
     * Ends the conversation and re-initializes input listener in the terminal.
     */
    end() {
        this.terminal.inputHandler.initInputListener();
    }

    /**
     * Handles the user's response to the current question.
     * @param {string} response - The user's response to the current question.
     */
    handleResponse(response) {
        // Check if the provided response matches one of the possible actions
        const action = this.currentQuestion.responses[response];
        
        if (action) {
            action.call(this); // Call the appropriate action for the given response
            // End the conversation after the action has been taken
            this.terminal.inputHandler.endConversation();
        } else {
            // Display an error message if the user's response doesn't match any of the provided options
            this.terminal.outputHandler.displayOutput(response, 'You must answer with one of the provided options.');
            this.start(); // Restart the conversation
        }
    }
}
