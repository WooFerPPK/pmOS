/**
 * Represents a Question with its prompt and associated responses.
 * 
 * @class
 */
export class Question {
    /**
     * Creates a new Question instance.
     *
     * @param {string} prompt - The main text or question to be presented.
     * @param {Array<string>} responses - A list of possible responses for the question.
     */
    constructor(prompt, responses) {
        // The question or statement to be displayed.
        this.prompt = prompt;

        // The potential responses for this question.
        this.responses = responses;
    }
}