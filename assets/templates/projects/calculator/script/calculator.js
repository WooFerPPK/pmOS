import { CalculatingEngine } from "/assets/js/modules/Calculator/CalculatingEngine.js";

const OPERATORS = ['+', '-', '*', '/', '='];

export default class Calculator {
    constructor(className, shadowRoot) {
        this.shadowRoot = shadowRoot;
        this.calculatorElement = this.shadowRoot.querySelector(`.${className}`);
        
        this.initializeComponents();
    }

    initializeComponents() {
        this.display = new Display(this.calculatorElement);
        this.input = new Input(this.calculatorElement);
        this.input.registerDisplayObserver(this.display);
        this.display.reset();
    }
}

class Input {
    constructor(calculatorElement) {
        this.calculatorElement = calculatorElement;
        this.engine = new CalculatingEngine();
        this.calculationHistory = new CalculationHistory();

        this.setupEventListeners();
    }

    setupEventListeners() {
        this.calculatorElement.querySelector('.numbers').addEventListener('click', this.handleNumberClick.bind(this));
        this.calculatorElement.querySelector('.operators').addEventListener('click', this.handleOperatorClick.bind(this));
        
    }

    handleNumberClick(event) {
        const target = event.target;
        if (target.classList.contains('number')) {
            this.calculationHistory.addEntry(target.value);
        }
    }

    handleOperatorClick(event) {
        const target = event.target;
        if (target.classList.contains('operator')) {
            this.processOperator(target);
        }
    }

    processOperator(target) {
        if (target.value === 'equal') {
            this.handleEquals();
        } else if (target.value === 'back') {
            this.calculationHistory.removeLastEntry();
        } else if (target.value === 'allclear') {
            this.handleAllClear();
        } else {
            this.calculationHistory.addEntry(target.textContent);
        }
    }

    handleAllClear() {
        this.calculationHistory.clearHistory();
    }

    handleEquals() {
        const currentCalculation = this.calculationHistory.getCurrentCalculation();
        
        if (currentCalculation.length === 0) return;
    
        if (OPERATORS.includes(currentCalculation[currentCalculation.length - 1])) {
            for (let i = currentCalculation.length - 2; i >= 0; i--) {
                if (!OPERATORS.includes(currentCalculation[i])) {
                    this.calculationHistory.addEntry(currentCalculation[i]);
                    break;
                }
            }
        }
    
        const expressionSinceLastEquals = this.calculationHistory.getCurrentCalculation();
        if (!expressionSinceLastEquals.every(e => OPERATORS.includes(e) || e === '=')) {
            const result = this.engine.evaluate(expressionSinceLastEquals.join(''));
            this.calculationHistory.addEntry('=');
            this.calculationHistory.addEntry(result);
            this.calculationHistory.startNewCalculationWithResult(result);
        }
    }
    

    registerDisplayObserver(display) {
        this.calculationHistory.registerObserver(display);
    }
}

class Display {
    constructor(calculatorElement) {
        this.displayElement = calculatorElement.querySelector('.display');
    }

    update(value) {
        this.displayElement.textContent = value || '0';
    }

    reset() {
        this.displayElement.textContent = '0';
    }
}

class CalculationHistory {
    constructor() {
        this.history = [[]];
        this.observers = [];
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
        this.history.push([result]);  // Start a new calculation array with the result
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
            OPERATORS.includes(entry) ? ` ${entry} ` : entry).join('')).join(', ').trim();
    }

    getCurrentCalculation() {
        return this.history[this.history.length - 1];
    }

    clearHistory() {
        this.history = [[]];
        this.notifyObservers();
    }
}