/**
 * Represents a user-defined action that can be executed.
 */
export class Action {
    /**
     * Constructs an Action with the provided run function.
     * @param {function} runFunction - The function to be executed when the action is run.
     */
    constructor(runFunction) {
        /*         * The function to execute when the action is run.
         * @type {function}
         */
        this.runFunction = runFunction;
    }
    
    /**
     * Executes the defined run function for the action.
     * @returns {*} The result of executing the run function.
     */
    run() {
        return this.runFunction();
    }
}
