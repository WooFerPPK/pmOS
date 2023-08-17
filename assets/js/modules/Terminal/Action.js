export class Action {
    constructor(runFunction) {
        this.runFunction = runFunction;
    }
    
    run() {
        return this.runFunction();
    }
}