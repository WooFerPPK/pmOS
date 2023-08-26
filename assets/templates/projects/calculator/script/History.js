class History {
    constructor(operators) {
        this.history = [[]];
        this.observers = [];
        this.operators = operators
    }

    addEntry(entry) {
        const currentCalculation = this.history[this.history.length - 1];
        currentCalculation.push(entry);
        this.notifyObservers();
    }

    addEntries(entries) {
        const currentCalculation = this.history[this.history.length - 1];
        currentCalculation.push(...entries);
        this.notifyObservers();
    }

    removeLastEntry() {
        const currentCalculation = this.history[this.history.length - 1];
        currentCalculation.pop();
        this.notifyObservers();
    }

    startNewCalculationWithResult(result) {
        this.history.push([result]); // Start a new calculation array with the result
    }

    registerObserver(observer) {
        this.observers.push(observer);
    }

    notifyObservers() {
        for (let observer of this.observers) {
            observer.update(this.toString());
        }
    }

    toString() {
        return this.history.map(calculation => calculation.map(entry => 
            this.operators.includes(entry) ? ` ${entry} ` : entry).join('')).join(', ').trim();
    }

    getCurrentCalculation() {
        return this.history[this.history.length - 1];
    }

    clearHistory() {
        this.history = [[]];
        this.notifyObservers();
    }
}

export default History;